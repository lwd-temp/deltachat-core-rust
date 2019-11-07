use std::net;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Condvar, Mutex, RwLock,
};
use std::time::{Duration, SystemTime};

use crate::configure::dc_connect_to_configured_imap;
use crate::constants::*;
use crate::context::Context;
use crate::dc_receive_imf::dc_receive_imf;
use crate::error::Error;
use crate::events::Event;
use crate::job::{connect_to_inbox, job_add, Action};
use crate::login_param::{dc_build_tls, CertificateChecks, LoginParam};
use crate::message::{self, update_msg_move_state, update_server_uid};
use crate::oauth2::dc_get_oauth2_access_token;
use crate::param::Params;
use crate::stock::StockMessage;
use crate::wrapmime;

const DC_IMAP_SEEN: usize = 0x0001;
const DCC_IMAP_DEBUG: &str = "DCC_IMAP_DEBUG";

#[derive(Debug, Display, Clone, Copy, PartialEq, Eq)]
pub enum ImapResult {
    Failed,
    RetryLater,
    AlreadyDone,
    Success,
}

const PREFETCH_FLAGS: &str = "(UID ENVELOPE)";
const BODY_FLAGS: &str = "(FLAGS BODY.PEEK[])";
const SELECT_ALL: &str = "1:*";

#[derive(Debug)]
pub struct Imap {
    config: Arc<RwLock<ImapConfig>>,
    watch: Arc<(Mutex<bool>, Condvar)>,

    session: Arc<Mutex<Option<Session>>>,
    stream: Arc<RwLock<Option<net::TcpStream>>>,
    connected: Arc<Mutex<bool>>,

    should_reconnect: AtomicBool,
}

#[derive(Debug)]
struct OAuth2 {
    user: String,
    access_token: String,
}

impl imap::Authenticator for OAuth2 {
    type Response = String;

    #[allow(unused_variables)]
    fn process(&self, data: &[u8]) -> Self::Response {
        format!(
            "user={}\x01auth=Bearer {}\x01\x01",
            self.user, self.access_token
        )
    }
}

#[derive(Debug)]
enum FolderMeaning {
    Unknown,
    SentObjects,
    Other,
}

#[derive(Debug)]
enum Client {
    Secure(
        imap::Client<native_tls::TlsStream<net::TcpStream>>,
        net::TcpStream,
    ),
    Insecure(imap::Client<net::TcpStream>, net::TcpStream),
}

#[derive(Debug)]
enum Session {
    Secure(imap::Session<native_tls::TlsStream<net::TcpStream>>),
    Insecure(imap::Session<net::TcpStream>),
}

#[derive(Debug)]
enum IdleHandle<'a> {
    Secure(imap::extensions::idle::Handle<'a, native_tls::TlsStream<net::TcpStream>>),
    Insecure(imap::extensions::idle::Handle<'a, net::TcpStream>),
}

impl<'a> IdleHandle<'a> {
    pub fn set_keepalive(&mut self, interval: Duration) {
        match self {
            IdleHandle::Secure(i) => i.set_keepalive(interval),
            IdleHandle::Insecure(i) => i.set_keepalive(interval),
        }
    }

    pub fn wait_keepalive(self) -> imap::error::Result<bool> {
        match self {
            IdleHandle::Secure(i) => i.wait_keepalive(),
            IdleHandle::Insecure(i) => i.wait_keepalive(),
        }
    }
}

impl Client {
    pub fn connect_secure<A: net::ToSocketAddrs, S: AsRef<str>>(
        addr: A,
        domain: S,
        certificate_checks: CertificateChecks,
    ) -> imap::error::Result<Self> {
        let stream = net::TcpStream::connect(addr)?;
        let tls = dc_build_tls(certificate_checks).unwrap();

        let s = stream.try_clone().expect("cloning the stream failed");
        let tls_stream = native_tls::TlsConnector::connect(&tls, domain.as_ref(), s)?;

        let mut client = imap::Client::new(tls_stream);
        if std::env::var(DCC_IMAP_DEBUG).is_ok() {
            client.debug = true;
        }
        client.read_greeting()?;

        Ok(Client::Secure(client, stream))
    }

    pub fn connect_insecure<A: net::ToSocketAddrs>(addr: A) -> imap::error::Result<Self> {
        let stream = net::TcpStream::connect(addr)?;

        let mut client = imap::Client::new(stream.try_clone().unwrap());
        if std::env::var(DCC_IMAP_DEBUG).is_ok() {
            client.debug = true;
        }
        client.read_greeting()?;

        Ok(Client::Insecure(client, stream))
    }

    pub fn secure<S: AsRef<str>>(
        self,
        domain: S,
        certificate_checks: CertificateChecks,
    ) -> imap::error::Result<Client> {
        match self {
            Client::Insecure(client, stream) => {
                let tls = dc_build_tls(certificate_checks).unwrap();

                let client_sec = client.secure(domain, &tls)?;

                Ok(Client::Secure(client_sec, stream))
            }
            // Nothing to do
            Client::Secure(_, _) => Ok(self),
        }
    }

    pub fn authenticate<A: imap::Authenticator, S: AsRef<str>>(
        self,
        auth_type: S,
        authenticator: &A,
    ) -> Result<(Session, net::TcpStream), (imap::error::Error, Client)> {
        match self {
            Client::Secure(i, stream) => match i.authenticate(auth_type, authenticator) {
                Ok(session) => Ok((Session::Secure(session), stream)),
                Err((err, c)) => Err((err, Client::Secure(c, stream))),
            },
            Client::Insecure(i, stream) => match i.authenticate(auth_type, authenticator) {
                Ok(session) => Ok((Session::Insecure(session), stream)),
                Err((err, c)) => Err((err, Client::Insecure(c, stream))),
            },
        }
    }

    pub fn login<U: AsRef<str>, P: AsRef<str>>(
        self,
        username: U,
        password: P,
    ) -> Result<(Session, net::TcpStream), (imap::error::Error, Client)> {
        match self {
            Client::Secure(i, stream) => match i.login(username, password) {
                Ok(session) => Ok((Session::Secure(session), stream)),
                Err((err, c)) => Err((err, Client::Secure(c, stream))),
            },
            Client::Insecure(i, stream) => match i.login(username, password) {
                Ok(session) => Ok((Session::Insecure(session), stream)),
                Err((err, c)) => Err((err, Client::Insecure(c, stream))),
            },
        }
    }
}

impl Session {
    pub fn capabilities(
        &mut self,
    ) -> imap::error::Result<imap::types::ZeroCopy<imap::types::Capabilities>> {
        match self {
            Session::Secure(i) => i.capabilities(),
            Session::Insecure(i) => i.capabilities(),
        }
    }

    pub fn list(
        &mut self,
        reference_name: Option<&str>,
        mailbox_pattern: Option<&str>,
    ) -> imap::error::Result<imap::types::ZeroCopy<Vec<imap::types::Name>>> {
        match self {
            Session::Secure(i) => i.list(reference_name, mailbox_pattern),
            Session::Insecure(i) => i.list(reference_name, mailbox_pattern),
        }
    }

    pub fn create<S: AsRef<str>>(&mut self, mailbox_name: S) -> imap::error::Result<()> {
        match self {
            Session::Secure(i) => i.create(mailbox_name),
            Session::Insecure(i) => i.create(mailbox_name),
        }
    }

    pub fn subscribe<S: AsRef<str>>(&mut self, mailbox: S) -> imap::error::Result<()> {
        match self {
            Session::Secure(i) => i.subscribe(mailbox),
            Session::Insecure(i) => i.subscribe(mailbox),
        }
    }

    pub fn close(&mut self) -> imap::error::Result<()> {
        match self {
            Session::Secure(i) => i.close(),
            Session::Insecure(i) => i.close(),
        }
    }

    pub fn select<S: AsRef<str>>(
        &mut self,
        mailbox_name: S,
    ) -> imap::error::Result<imap::types::Mailbox> {
        match self {
            Session::Secure(i) => i.select(mailbox_name),
            Session::Insecure(i) => i.select(mailbox_name),
        }
    }

    pub fn fetch<S1, S2>(
        &mut self,
        sequence_set: S1,
        query: S2,
    ) -> imap::error::Result<imap::types::ZeroCopy<Vec<imap::types::Fetch>>>
    where
        S1: AsRef<str>,
        S2: AsRef<str>,
    {
        match self {
            Session::Secure(i) => i.fetch(sequence_set, query),
            Session::Insecure(i) => i.fetch(sequence_set, query),
        }
    }

    pub fn uid_fetch<S1, S2>(
        &mut self,
        uid_set: S1,
        query: S2,
    ) -> imap::error::Result<imap::types::ZeroCopy<Vec<imap::types::Fetch>>>
    where
        S1: AsRef<str>,
        S2: AsRef<str>,
    {
        match self {
            Session::Secure(i) => i.uid_fetch(uid_set, query),
            Session::Insecure(i) => i.uid_fetch(uid_set, query),
        }
    }

    pub fn idle(&mut self) -> imap::error::Result<IdleHandle> {
        match self {
            Session::Secure(i) => i.idle().map(IdleHandle::Secure),
            Session::Insecure(i) => i.idle().map(IdleHandle::Insecure),
        }
    }

    pub fn uid_store<S1, S2>(
        &mut self,
        uid_set: S1,
        query: S2,
    ) -> imap::error::Result<imap::types::ZeroCopy<Vec<imap::types::Fetch>>>
    where
        S1: AsRef<str>,
        S2: AsRef<str>,
    {
        match self {
            Session::Secure(i) => i.uid_store(uid_set, query),
            Session::Insecure(i) => i.uid_store(uid_set, query),
        }
    }

    pub fn uid_mv<S1: AsRef<str>, S2: AsRef<str>>(
        &mut self,
        uid_set: S1,
        mailbox_name: S2,
    ) -> imap::error::Result<()> {
        match self {
            Session::Secure(i) => i.uid_mv(uid_set, mailbox_name),
            Session::Insecure(i) => i.uid_mv(uid_set, mailbox_name),
        }
    }

    pub fn uid_copy<S1: AsRef<str>, S2: AsRef<str>>(
        &mut self,
        uid_set: S1,
        mailbox_name: S2,
    ) -> imap::error::Result<()> {
        match self {
            Session::Secure(i) => i.uid_copy(uid_set, mailbox_name),
            Session::Insecure(i) => i.uid_copy(uid_set, mailbox_name),
        }
    }
}

#[derive(Debug)]
struct ImapConfig {
    pub addr: String,
    pub imap_server: String,
    pub imap_port: u16,
    pub imap_user: String,
    pub imap_pw: String,
    pub certificate_checks: CertificateChecks,
    pub server_flags: usize,
    pub selected_folder: Option<String>,
    pub selected_mailbox: Option<imap::types::Mailbox>,
    pub selected_folder_needs_expunge: bool,
    pub can_idle: bool,
    pub has_xlist: bool,
    pub imap_delimiter: char,
    pub watch_folder: Option<String>,
}

impl Default for ImapConfig {
    fn default() -> Self {
        ImapConfig {
            addr: "".into(),
            imap_server: "".into(),
            imap_port: 0,
            imap_user: "".into(),
            imap_pw: "".into(),
            certificate_checks: Default::default(),
            server_flags: 0,
            selected_folder: None,
            selected_mailbox: None,
            selected_folder_needs_expunge: false,
            can_idle: false,
            has_xlist: false,
            imap_delimiter: '.',
            watch_folder: None,
        }
    }
}

impl Imap {
    pub fn new() -> Self {
        Imap {
            session: Arc::new(Mutex::new(None)),
            stream: Arc::new(RwLock::new(None)),
            config: Arc::new(RwLock::new(ImapConfig::default())),
            watch: Arc::new((Mutex::new(false), Condvar::new())),
            connected: Arc::new(Mutex::new(false)),
            should_reconnect: AtomicBool::new(false),
        }
    }

    pub fn is_connected(&self) -> bool {
        *self.connected.lock().unwrap()
    }

    pub fn should_reconnect(&self) -> bool {
        self.should_reconnect.load(Ordering::Relaxed)
    }

    fn setup_handle_if_needed(&self, context: &Context) -> bool {
        if self.config.read().unwrap().imap_server.is_empty() {
            return false;
        }

        if self.should_reconnect() {
            self.unsetup_handle(context);
        }

        if self.is_connected() && self.stream.read().unwrap().is_some() {
            self.should_reconnect.store(false, Ordering::Relaxed);
            return true;
        }

        let server_flags = self.config.read().unwrap().server_flags as i32;

        let connection_res: imap::error::Result<Client> =
            if (server_flags & (DC_LP_IMAP_SOCKET_STARTTLS | DC_LP_IMAP_SOCKET_PLAIN)) != 0 {
                let config = self.config.read().unwrap();
                let imap_server: &str = config.imap_server.as_ref();
                let imap_port = config.imap_port;

                Client::connect_insecure((imap_server, imap_port)).and_then(|client| {
                    if (server_flags & DC_LP_IMAP_SOCKET_STARTTLS) != 0 {
                        client.secure(imap_server, config.certificate_checks)
                    } else {
                        Ok(client)
                    }
                })
            } else {
                let config = self.config.read().unwrap();
                let imap_server: &str = config.imap_server.as_ref();
                let imap_port = config.imap_port;

                Client::connect_secure(
                    (imap_server, imap_port),
                    imap_server,
                    config.certificate_checks,
                )
            };

        let login_res = match connection_res {
            Ok(client) => {
                let config = self.config.read().unwrap();
                let imap_user: &str = config.imap_user.as_ref();
                let imap_pw: &str = config.imap_pw.as_ref();

                if (server_flags & DC_LP_AUTH_OAUTH2) != 0 {
                    let addr: &str = config.addr.as_ref();

                    if let Some(token) = dc_get_oauth2_access_token(context, addr, imap_pw, true) {
                        let auth = OAuth2 {
                            user: imap_user.into(),
                            access_token: token,
                        };
                        client.authenticate("XOAUTH2", &auth)
                    } else {
                        return false;
                    }
                } else {
                    client.login(imap_user, imap_pw)
                }
            }
            Err(err) => {
                let config = self.config.read().unwrap();
                let imap_server: &str = config.imap_server.as_ref();
                let imap_port = config.imap_port;
                let message = context.stock_string_repl_str2(
                    StockMessage::ServerResponse,
                    format!("{}:{}", imap_server, imap_port),
                    format!("{}", err),
                );

                emit_event!(context, Event::ErrorNetwork(message));

                return false;
            }
        };

        self.should_reconnect.store(false, Ordering::Relaxed);

        match login_res {
            Ok((session, stream)) => {
                *self.session.lock().unwrap() = Some(session);
                *self.stream.write().unwrap() = Some(stream);
                true
            }
            Err((err, _)) => {
                let imap_user = self.config.read().unwrap().imap_user.to_owned();
                let message = context.stock_string_repl_str(StockMessage::CannotLogin, &imap_user);

                emit_event!(
                    context,
                    Event::ErrorNetwork(format!("{} ({})", message, err))
                );
                self.unsetup_handle(context);

                false
            }
        }
    }

    fn unsetup_handle(&self, context: &Context) {
        info!(context, "IMAP unsetup_handle step 1 (closing down stream).");
        let stream = self.stream.write().unwrap().take();
        if let Some(stream) = stream {
            if let Err(err) = stream.shutdown(net::Shutdown::Both) {
                warn!(context, "failed to shutdown connection: {:?}", err);
            }
        }

        info!(
            context,
            "IMAP unsetup_handle step 2 (acquiring session.lock)"
        );
        if let Some(mut session) = self.session.lock().unwrap().take() {
            if let Err(err) = session.close() {
                warn!(context, "failed to close connection: {:?}", err);
            }
        }

        info!(context, "IMAP unsetup_handle step 3 (clearing config).");
        self.config.write().unwrap().selected_folder = None;
        self.config.write().unwrap().selected_mailbox = None;
        info!(context, "IMAP unsetup_handle step 4 (disconnected).",);
    }

    fn free_connect_params(&self) {
        let mut cfg = self.config.write().unwrap();

        cfg.addr = "".into();
        cfg.imap_server = "".into();
        cfg.imap_user = "".into();
        cfg.imap_pw = "".into();
        cfg.imap_port = 0;

        cfg.can_idle = false;
        cfg.has_xlist = false;

        cfg.watch_folder = None;
    }

    pub fn connect(&self, context: &Context, lp: &LoginParam) -> bool {
        if lp.mail_server.is_empty() || lp.mail_user.is_empty() || lp.mail_pw.is_empty() {
            return false;
        }

        if self.is_connected() {
            return true;
        }

        {
            let addr = &lp.addr;
            let imap_server = &lp.mail_server;
            let imap_port = lp.mail_port as u16;
            let imap_user = &lp.mail_user;
            let imap_pw = &lp.mail_pw;
            let server_flags = lp.server_flags as usize;

            let mut config = self.config.write().unwrap();
            config.addr = addr.to_string();
            config.imap_server = imap_server.to_string();
            config.imap_port = imap_port;
            config.imap_user = imap_user.to_string();
            config.imap_pw = imap_pw.to_string();
            config.certificate_checks = lp.imap_certificate_checks;
            config.server_flags = server_flags;
        }

        if !self.setup_handle_if_needed(context) {
            self.free_connect_params();
            return false;
        }

        let (teardown, can_idle, has_xlist) = match &mut *self.session.lock().unwrap() {
            Some(ref mut session) => match session.capabilities() {
                Ok(caps) => {
                    if !context.sql.is_open() {
                        warn!(context, "IMAP-LOGIN as {} ok but ABORTING", lp.mail_user,);
                        (true, false, false)
                    } else {
                        let can_idle = caps.has_str("IDLE");
                        let has_xlist = caps.has_str("XLIST");
                        let caps_list = caps
                            .iter()
                            .fold(String::new(), |s, c| s + &format!(" {:?}", c));
                        emit_event!(
                            context,
                            Event::ImapConnected(format!(
                                "IMAP-LOGIN as {}, capabilities: {}",
                                lp.mail_user, caps_list,
                            ))
                        );
                        (false, can_idle, has_xlist)
                    }
                }
                Err(err) => {
                    info!(context, "CAPABILITY command error: {}", err);
                    (true, false, false)
                }
            },
            None => (true, false, false),
        };

        if teardown {
            self.unsetup_handle(context);
            self.free_connect_params();
            false
        } else {
            self.config.write().unwrap().can_idle = can_idle;
            self.config.write().unwrap().has_xlist = has_xlist;
            *self.connected.lock().unwrap() = true;
            true
        }
    }

    pub fn disconnect(&self, context: &Context) {
        if self.is_connected() {
            self.unsetup_handle(context);
            self.free_connect_params();
            *self.connected.lock().unwrap() = false;
        }
    }

    pub fn set_watch_folder(&self, watch_folder: String) {
        self.config.write().unwrap().watch_folder = Some(watch_folder);
    }

    pub fn fetch(&self, context: &Context) -> bool {
        if !self.is_connected() || !context.sql.is_open() {
            return false;
        }

        self.setup_handle_if_needed(context);

        let watch_folder = self.config.read().unwrap().watch_folder.to_owned();

        if let Some(ref watch_folder) = watch_folder {
            // as during the fetch commands, new messages may arrive, we fetch until we do not
            // get any more. if IDLE is called directly after, there is only a small chance that
            // messages are missed and delayed until the next IDLE call
            loop {
                if self.fetch_from_single_folder(context, watch_folder) == 0 {
                    break;
                }
            }
            true
        } else {
            false
        }
    }

    fn expunge_folder(&self, context: &Context) -> bool {
        if let Some(ref folder) = self.config.read().unwrap().selected_folder {
            info!(context, "Expunge messages in \"{}\".", folder);

            // A CLOSE-SELECT is considerably faster than an EXPUNGE-SELECT, see
            // https://tools.ietf.org/html/rfc3501#section-6.4.2
            if let Some(ref mut session) = &mut *self.session.lock().unwrap() {
                match session.close() {
                    Ok(_) => {
                        info!(context, "close/expunge succeeded");
                    }
                    Err(err) => {
                        warn!(context, "failed to close session: {:?}", err);
                        return false;
                    }
                }
            } else {
                unreachable!();
            }
        }

        true
    }

    fn select_folder(&self, context: &Context, folder: &str) -> usize {
        if self.session.lock().unwrap().is_none() {
            let mut cfg = self.config.write().unwrap();
            cfg.selected_folder = None;
            cfg.selected_folder_needs_expunge = false;
            return 0;
        }

        // if there is a new folder and the new folder is equal to the selected one, there's nothing to do.
        // if there is _no_ new folder, we continue as we might want to expunge below.
        if let Some(ref selected_folder) = self.config.read().unwrap().selected_folder {
            if folder == selected_folder {
                return 1;
            }
        }

        // deselect existing folder, if needed (it's also done implicitly by SELECT, however, without EXPUNGE then)
        let needs_expunge = { self.config.read().unwrap().selected_folder_needs_expunge };
        if needs_expunge {
            self.expunge_folder(context);
            self.config.write().unwrap().selected_folder_needs_expunge = false;
        }

        // select new folder
        if let Some(ref mut session) = &mut *self.session.lock().unwrap() {
            match session.select(&folder) {
                Ok(mailbox) => {
                    let mut config = self.config.write().unwrap();
                    config.selected_folder = Some(folder.to_string());
                    config.selected_mailbox = Some(mailbox);
                }
                Err(err) => {
                    info!(
                        context,
                        "Cannot select folder: {}; {:?}.",
                        folder,
                        err
                    );

                    self.config.write().unwrap().selected_folder = None;
                    self.should_reconnect.store(true, Ordering::Relaxed);
                    return 0;
                }
            }
        } else {
            unreachable!();
        }

        1
    }

    fn get_config_last_seen_uid(&self, context: &Context, folder: &str) -> (u32, u32) {
        let key = format!("imap.mailbox.{}", folder);
        if let Some(entry) = context.sql.get_raw_config(context, &key) {
            // the entry has the format `imap.mailbox.<folder>=<uidvalidity>:<lastseenuid>`
            let mut parts = entry.split(':');
            (
                parts
                    .next()
                    .unwrap_or_default()
                    .parse()
                    .unwrap_or_else(|_| 0),
                parts
                    .next()
                    .unwrap_or_default()
                    .parse()
                    .unwrap_or_else(|_| 0),
            )
        } else {
            (0, 0)
        }
    }

    fn fetch_from_single_folder(&self, context: &Context, folder: &str) -> usize {
        if !self.is_connected() {
            info!(
                context,
                "Cannot fetch from \"{}\" - not connected.",
                folder
            );

            return 0;
        }

        if self.select_folder(context, &folder) == 0 {
            info!(
                context,
                "Cannot select folder \"{}\" for fetching.",
                folder
            );

            return 0;
        }

        // compare last seen UIDVALIDITY against the current one
        let (mut uid_validity, mut last_seen_uid) = self.get_config_last_seen_uid(context, &folder);
        /*

        let uid_validity = self.config.read().unwrap().selected_mailbox.as_ref().expect("just selected").uid_validity;

        if uid_validity.is_none() {
            error!(
                context,
                "Cannot get UIDVALIDITY for folder \"{}\".",
                folder.as_ref(),
            );

            return 0;
        }

        if .uid_validity.unwrap_or_default() != uid_validity {
            // first time this folder is selected or UIDVALIDITY has changed, init lastseenuid and save it to config

            if mailbox.exists == 0 {
                info!(context, "Folder \"{}\" is empty.", folder.as_ref());

                // set lastseenuid=0 for empty folders.
                // id we do not do this here, we'll miss the first message
                // as we will get in here again and fetch from lastseenuid+1 then

                self.set_config_last_seen_uid(
                    context,
                    &folder,
                    mailbox.uid_validity.unwrap_or_default(),
                    0,
                );
                return 0;
            }

            let list = if let Some(ref mut session) = &mut *self.session.lock().unwrap() {
                // `FETCH <message sequence number> (UID)`
                let set = format!("{}", mailbox.exists);
                match session.fetch(set, PREFETCH_FLAGS) {
                    Ok(list) => list,
                    Err(_err) => {
                        self.should_reconnect.store(true, Ordering::Relaxed);
                        info!(
                            context,
                            "No result returned for folder \"{}\".",
                            folder.as_ref()
                        );

                        return 0;
                    }
                }
            } else {
                return 0;
            };

            last_seen_uid = list[0].uid.unwrap_or_else(|| 0);

            // if the UIDVALIDITY has _changed_, decrease lastseenuid by one to avoid gaps (well add 1 below
            if uid_validity > 0 && last_seen_uid > 1 {
                last_seen_uid -= 1;
            }

            uid_validity = mailbox.uid_validity.unwrap_or_default();
            self.set_config_last_seen_uid(context, &folder, uid_validity, last_seen_uid);
            info!(
                context,
                "lastseenuid initialized to {} for {}@{}",
                last_seen_uid,
                folder.as_ref(),
                uid_validity,
            );
        }
        */

        let mut read_cnt = 0;
        let mut read_errors = 0;
        let mut new_last_seen_uid = 0;

        let list = if let Some(ref mut session) = &mut *self.session.lock().unwrap() {
            // fetch messages with larger UID than the last one seen
            // (`UID FETCH lastseenuid+1:*)`, see RFC 4549
            let set = format!("{}:*", last_seen_uid + 1);
            match session.uid_fetch(set, PREFETCH_FLAGS) {
                Ok(list) => list,
                Err(err) => {
                    warn!(context, "failed to fetch uids: {}", err);
                    return 0;
                }
            }
        } else {
            return 0;
        };

        // go through all mails in folder (this is typically _fast_ as we already have the whole list)
        for msg in &list {
            let cur_uid = msg.uid.unwrap_or_else(|| 0);
            if cur_uid > last_seen_uid {
                read_cnt += 1;

                let message_id = prefetch_get_message_id(msg).unwrap_or_default();

                if !precheck_imf(context, &message_id, &folder, cur_uid) {
                    // check passed, go fetch the rest
                    if self.fetch_single_msg(context, &folder, cur_uid) == 0 {
                        info!(
                            context,
                            "Read error for message {} from \"{}\", trying over later.",
                            message_id,
                            folder
                        );

                        read_errors += 1;
                    }
                } else {
                    // check failed
                    info!(
                        context,
                        "Skipping message {} from \"{}\" by precheck.",
                        message_id,
                        folder
                    );
                }
                if cur_uid > new_last_seen_uid {
                    new_last_seen_uid = cur_uid
                }
            }
        }

        if 0 == read_errors && new_last_seen_uid > 0 {
            // TODO: it might be better to increase the lastseenuid also on partial errors.
            // however, this requires to sort the list before going through it above.
            self.set_config_last_seen_uid(context, &folder, uid_validity, new_last_seen_uid);
        }

        if read_errors > 0 {
            warn!(
                context,
                "{} mails read from \"{}\" with {} errors.",
                read_cnt,
                folder,
                read_errors
            );
        } else {
            info!(
                context,
                "{} mails read from \"{}\".",
                read_cnt,
                folder
            );
        }

        read_cnt
    }

    fn set_config_last_seen_uid(
        &self,
        context: &Context,
        folder: &str,
        uidvalidity: u32,
        lastseenuid: u32,
    ) {
        let key = format!("imap.mailbox.{}", folder);
        let val = format!("{}:{}", uidvalidity, lastseenuid);

        context.sql.set_raw_config(context, &key, Some(&val)).ok();
    }

    fn fetch_single_msg(
        &self,
        context: &Context,
        folder: &str,
        server_uid: u32,
    ) -> usize {
        // the function returns:
        // 0  the caller should try over again later
        // or  1  if the messages should be treated as received, the caller should not try to read the message again (even if no database entries are returned)
        if !self.is_connected() {
            return 0;
        }

        let set = format!("{}", server_uid);

        let msgs = if let Some(ref mut session) = &mut *self.session.lock().unwrap() {
            match session.uid_fetch(set, BODY_FLAGS) {
                Ok(msgs) => msgs,
                Err(err) => {
                    self.should_reconnect.store(true, Ordering::Relaxed);
                    warn!(
                        context,
                        "Error on fetching message #{} from folder \"{}\"; retry={}; error={}.",
                        server_uid,
                        folder,
                        self.should_reconnect(),
                        err
                    );
                    return 0;
                }
            }
        } else {
            return 1;
        };

        if msgs.is_empty() {
            warn!(
                context,
                "Message #{} does not exist in folder \"{}\".",
                server_uid,
                folder,
            );
        } else {
            let msg = &msgs[0];

            // XXX put flags into a set and pass them to dc_receive_imf
            let is_deleted = msg.flags().iter().any(|flag| match flag {
                imap::types::Flag::Deleted => true,
                _ => false,
            });
            let is_seen = msg.flags().iter().any(|flag| match flag {
                imap::types::Flag::Seen => true,
                _ => false,
            });

            let flags = if is_seen { DC_IMAP_SEEN } else { 0 };

            if !is_deleted && msg.body().is_some() {
                let body = msg.body().unwrap_or_default();
                unsafe {
                    dc_receive_imf(context, &body, &folder, server_uid, flags as u32);
                }
            }
        }

        1
    }

    pub fn idle(&self, context: &Context) {
        if !self.config.read().unwrap().can_idle {
            return self.fake_idle(context);
        }

        self.setup_handle_if_needed(context);

        let watch_folder = self.config.read().unwrap().watch_folder.clone();
        if let Some(wfolder) = watch_folder {
            if self.select_folder(context, &wfolder) == 0 {
                warn!(context, "IMAP-IDLE not setup.",);
                return self.fake_idle(context);
            }
        }

        let session = self.session.clone();
        let mut worker = Some({
            let (sender, receiver) = std::sync::mpsc::channel();
            let v = self.watch.clone();

            info!(context, "IMAP-IDLE SPAWNING");
            std::thread::spawn(move || {
                let &(ref lock, ref cvar) = &*v;
                if let Some(ref mut session) = &mut *session.lock().unwrap() {
                    loop {
                        let res = match session.idle() {
                            Ok(mut idle) => {
                                // most servers do not allow more than ~28 minutes; stay clearly below that.
                                // a good value that is also used by other MUAs is 23 minutes.
                                // if needed, the ui can call dc_imap_interrupt_idle() to trigger a reconnect.
                                // idle.set_keepalive(Duration::from_secs(23 * 60));
                                idle.set_keepalive(Duration::from_secs(23 * 60));
                                idle.wait_keepalive()
                            }
                            Err(err) => {
                                let _ = sender.send(Err(imap::error::Error::Bad(err.to_string())));
                                break;
                            }
                        };
                        match res {
                            Ok(true) => {
                                let _ = sender.send(Ok(()));
                                break;
                            }
                            Ok(false) => {} // continue loop
                            Err(err) => {
                                let _ = sender.send(Err(imap::error::Error::Bad(format!(
                                    "wait_keepalive failed {}",
                                    err
                                ))));
                                break;
                            }
                        }
                    }
                    // Trigger condvar
                    let mut watch = lock.lock().unwrap();
                    *watch = true;
                    cvar.notify_one();
                }
            });
            receiver
        });

        let &(ref lock, ref cvar) = &*self.watch.clone();
        let mut watch = lock.lock().unwrap();

        let handle_res = |res| match res {
            Ok(()) => {
                info!(context, "IMAP-IDLE has data.");
            }
            Err(err) => match err {
                imap::error::Error::Io(_)
                | imap::error::Error::ConnectionLost
                | imap::error::Error::Bad(_) => {
                    info!(context, "IMAP-IDLE wait cancelled, we will reconnect soon.");
                    self.unsetup_handle(context);
                    info!(context, "IMAP-IDLE has SHUTDOWN");
                    self.should_reconnect.store(true, Ordering::Relaxed);
                }
                _ => {
                    warn!(context, "IMAP-IDLE returns unknown value: {}", err);
                }
            },
        };

        loop {
            if let Ok(res) = worker.as_ref().unwrap().try_recv() {
                handle_res(res);
                break;
            } else {
                let res = cvar.wait(watch).unwrap();
                watch = res;
                if *watch {
                    if let Ok(res) = worker.as_ref().unwrap().try_recv() {
                        handle_res(res);
                    } else {
                        info!(context, "IMAP-IDLE interrupted");
                    }

                    drop(worker.take());
                    break;
                }
            }
        }

        *watch = false;
    }

    fn fake_idle(&self, context: &Context) {
        // Idle using timeouts. This is also needed if we're not yet configured -
        // in this case, we're waiting for a configure job
        let fake_idle_start_time = SystemTime::now();
        let mut wait_long = false;

        info!(context, "IMAP-fake-IDLEing...");

        let mut do_fake_idle = true;
        while do_fake_idle {
            // wait a moment: every 5 seconds in the first 3 minutes after a new message, after that every 60 seconds.
            let seconds_to_wait = if fake_idle_start_time.elapsed().unwrap_or_default()
                < Duration::new(3 * 60, 0)
                && !wait_long
            {
                Duration::new(5, 0)
            } else {
                Duration::new(60, 0)
            };

            let &(ref lock, ref cvar) = &*self.watch.clone();
            let mut watch = lock.lock().unwrap();

            loop {
                let res = cvar.wait_timeout(watch, seconds_to_wait).unwrap();
                watch = res.0;
                if *watch {
                    do_fake_idle = false;
                }
                if *watch || res.1.timed_out() {
                    break;
                }
            }

            *watch = false;

            if !do_fake_idle {
                return;
            }

            // check if we want to finish fake-idling.
            if !self.is_connected() {
                // try to connect with proper login params
                // (setup_handle_if_needed might not know about them if we
                // never successfully connected)
                if dc_connect_to_configured_imap(context, &self) != 0 {
                    return;
                }
                // we cannot connect, wait long next time (currently 60 secs, see above)
                wait_long = true;
                continue;
            }
            // we are connected, let's see if fetching messages results
            // in anything.  If so, we behave as if IDLE had data but
            // will have already fetched the messages so perform_*_fetch
            // will not find any new.

            let watch_folder = self.config.read().unwrap().watch_folder.clone();
            if let Some(watch_folder) = watch_folder {
                if 0 != self.fetch_from_single_folder(context, &watch_folder) {
                    do_fake_idle = false;
                }
            }
        }
    }

    pub fn interrupt_idle(&self) {
        // interrupt idle
        let &(ref lock, ref cvar) = &*self.watch.clone();
        let mut watch = lock.lock().unwrap();

        *watch = true;
        cvar.notify_one();
    }

    pub fn mv(
        &self,
        context: &Context,
        folder: &str,
        uid: u32,
        dest_folder: &str,
        dest_uid: &mut u32,
    ) -> ImapResult {
        if folder == dest_folder {
            info!(
                context,
                "Skip moving message; message {}/{} is already in {}...", folder, uid, dest_folder,
            );
            return ImapResult::AlreadyDone;
        }
        if let Some(imapresult) = self.prepare_imap_operation_on_msg(context, folder, uid) {
            return imapresult;
        }
        // we are connected, and the folder is selected

        // XXX Rust-Imap provides no target uid on mv, so just set it to 0
        *dest_uid = 0;

        let set = format!("{}", uid);
        let display_folder_id = format!("{}/{}", folder, uid);
        if let Some(ref mut session) = &mut *self.session.lock().unwrap() {
            match session.uid_mv(&set, &dest_folder) {
                Ok(_) => {
                    emit_event!(
                        context,
                        Event::ImapMessageMoved(format!(
                            "IMAP Message {} moved to {}",
                            display_folder_id, dest_folder
                        ))
                    );
                    return ImapResult::Success;
                }
                Err(err) => {
                    warn!(
                        context,
                        "Cannot move message, fallback to COPY/DELETE {}/{} to {}: {}",
                        folder,
                        uid,
                        dest_folder,
                        err
                    );
                }
            }
        } else {
            unreachable!();
        };

        if let Some(ref mut session) = &mut *self.session.lock().unwrap() {
            match session.uid_copy(&set, &dest_folder) {
                Ok(_) => {
                    if !self.add_flag_finalized(context, uid, "\\Deleted") {
                        warn!(context, "Cannot mark {} as \"Deleted\" after copy.", uid);
                        ImapResult::Failed
                    } else {
                        self.config.write().unwrap().selected_folder_needs_expunge = true;
                        ImapResult::Success
                    }
                }
                Err(err) => {
                    warn!(context, "Could not copy message: {}", err);
                    ImapResult::Failed
                }
            }
        } else {
            unreachable!();
        }
    }

    fn add_flag_finalized(&self, context: &Context, server_uid: u32, flag: &str) -> bool {
        // return true if we successfully set the flag or we otherwise
        // think add_flag should not be retried: Disconnection during setting
        // the flag, or other imap-errors, returns true as well.
        //
        // returning false means that the operation can be retried.
        if server_uid == 0 {
            return true; // might be moved but we don't want to have a stuck job
        }
        let s = server_uid.to_string();
        self.add_flag_finalized_with_set(context, &s, flag)
    }

    fn add_flag_finalized_with_set(&self, context: &Context, uid_set: &str, flag: &str) -> bool {
        if self.should_reconnect() {
            return false;
        }
        if let Some(ref mut session) = &mut *self.session.lock().unwrap() {
            let query = format!("+FLAGS ({})", flag);
            match session.uid_store(uid_set, &query) {
                Ok(_) => {}
                Err(err) => {
                    warn!(
                        context,
                        "IMAP failed to store: ({}, {}) {:?}", uid_set, query, err
                    );
                }
            }
            true // we tried once, that's probably enough for setting flag
        } else {
            unreachable!();
        }
    }

    pub fn prepare_imap_operation_on_msg(
        &self,
        context: &Context,
        folder: &str,
        uid: u32,
    ) -> Option<ImapResult> {
        if uid == 0 {
            return Some(ImapResult::Failed);
        } else if !self.is_connected() {
            connect_to_inbox(context, &self);
            if !self.is_connected() {
                return Some(ImapResult::RetryLater);
            }
        }
        if self.select_folder(context, &folder) == 0 {
            warn!(
                context,
                "Cannot select folder {} for preparing IMAP operation", folder
            );
            Some(ImapResult::RetryLater)
        } else {
            None
        }
    }

    pub fn set_seen(&self, context: &Context, folder: &str, uid: u32) -> ImapResult {
        if let Some(imapresult) = self.prepare_imap_operation_on_msg(context, folder, uid) {
            return imapresult;
        }
        // we are connected, and the folder is selected
        info!(context, "Marking message {}/{} as seen...", folder, uid,);

        if self.add_flag_finalized(context, uid, "\\Seen") {
            ImapResult::Success
        } else {
            warn!(
                context,
                "Cannot mark message {} in folder {} as seen, ignoring.", uid, folder
            );
            ImapResult::Failed
        }
    }

    // only returns 0 on connection problems; we should try later again in this case *
    pub fn delete_msg(
        &self,
        context: &Context,
        message_id: &str,
        folder: &str,
        uid: &mut u32,
    ) -> ImapResult {
        if let Some(imapresult) = self.prepare_imap_operation_on_msg(context, folder, *uid) {
            return imapresult;
        }
        // we are connected, and the folder is selected

        let set = format!("{}", uid);
        let display_imap_id = format!("{}/{}", folder, uid);

        // double-check that we are deleting the correct message-id
        // this comes at the expense of another imap query
        if let Some(ref mut session) = &mut *self.session.lock().unwrap() {
            match session.uid_fetch(set, PREFETCH_FLAGS) {
                Ok(msgs) => {
                    if msgs.is_empty() {
                        warn!(
                            context,
                            "Cannot delete on IMAP, {}: imap entry gone '{}'",
                            display_imap_id,
                            message_id,
                        );
                        return ImapResult::Failed;
                    }
                    let remote_message_id =
                        prefetch_get_message_id(msgs.first().unwrap()).unwrap_or_default();

                    if remote_message_id != message_id {
                        warn!(
                            context,
                            "Cannot delete on IMAP, {}: remote message-id '{}' != '{}'",
                            display_imap_id,
                            remote_message_id,
                            message_id,
                        );
                    }
                    *uid = 0;
                }
                Err(err) => {
                    warn!(
                        context,
                        "Cannot delete {} on IMAP: {}", display_imap_id, err
                    );
                    *uid = 0;
                }
            }
        }

        // mark the message for deletion
        if !self.add_flag_finalized(context, *uid, "\\Deleted") {
            warn!(
                context,
                "Cannot mark message {} as \"Deleted\".", display_imap_id
            );
            ImapResult::Failed
        } else {
            emit_event!(
                context,
                Event::ImapMessageDeleted(format!(
                    "IMAP Message {} marked as deleted [{}]",
                    display_imap_id, message_id
                ))
            );
            self.config.write().unwrap().selected_folder_needs_expunge = true;
            ImapResult::Success
        }
    }

    pub fn configure_folders(&self, context: &Context, flags: libc::c_int) {
        if !self.is_connected() {
            return;
        }

        info!(context, "Configuring IMAP-folders.");

        let folders = self.list_folders(context).unwrap();
        let delimiter = self.config.read().unwrap().imap_delimiter;
        let fallback_folder = format!("INBOX{}DeltaChat", delimiter);

        let mut mvbox_folder = folders
            .iter()
            .find(|folder| folder.name() == "DeltaChat" || folder.name() == fallback_folder)
            .map(|n| n.name().to_string());

        let sentbox_folder = folders
            .iter()
            .find(|folder| match get_folder_meaning(folder) {
                FolderMeaning::SentObjects => true,
                _ => false,
            });

        if mvbox_folder.is_none() && 0 != (flags as usize & DC_CREATE_MVBOX) {
            info!(context, "Creating MVBOX-folder \"DeltaChat\"...",);

            if let Some(ref mut session) = &mut *self.session.lock().unwrap() {
                match session.create("DeltaChat") {
                    Ok(_) => {
                        mvbox_folder = Some("DeltaChat".into());

                        info!(context, "MVBOX-folder created.",);
                    }
                    Err(err) => {
                        warn!(
                            context,
                            "Cannot create MVBOX-folder, using trying INBOX subfolder. ({})", err
                        );

                        match session.create(&fallback_folder) {
                            Ok(_) => {
                                mvbox_folder = Some(fallback_folder);
                                info!(
                                    context,
                                    "MVBOX-folder created as INBOX subfolder. ({})", err
                                );
                            }
                            Err(err) => {
                                warn!(context, "Cannot create MVBOX-folder. ({})", err);
                            }
                        }
                    }
                }
                // SUBSCRIBE is needed to make the folder visible to the LSUB command
                // that may be used by other MUAs to list folders.
                // for the LIST command, the folder is always visible.
                if let Some(ref mvbox) = mvbox_folder {
                    // TODO: better error handling
                    session.subscribe(mvbox).expect("failed to subscribe");
                }
            }
        }

        context
            .sql
            .set_raw_config_int(context, "folders_configured", 3)
            .ok();
        if let Some(ref mvbox_folder) = mvbox_folder {
            context
                .sql
                .set_raw_config(context, "configured_mvbox_folder", Some(mvbox_folder))
                .ok();
        }
        if let Some(ref sentbox_folder) = sentbox_folder {
            context
                .sql
                .set_raw_config(
                    context,
                    "configured_sentbox_folder",
                    Some(sentbox_folder.name()),
                )
                .ok();
        }
    }

    fn list_folders(
        &self,
        context: &Context,
    ) -> Option<imap::types::ZeroCopy<Vec<imap::types::Name>>> {
        if let Some(ref mut session) = &mut *self.session.lock().unwrap() {
            // TODO: use xlist when available
            match session.list(Some(""), Some("*")) {
                Ok(list) => {
                    if list.is_empty() {
                        warn!(context, "Folder list is empty.",);
                    }
                    Some(list)
                }
                Err(err) => {
                    eprintln!("list error: {:?}", err);
                    warn!(context, "Cannot get folder list.",);

                    None
                }
            }
        } else {
            None
        }
    }

    pub fn empty_folder(&self, context: &Context, folder: &str) {
        info!(context, "emptying folder {}", folder);

        if folder.is_empty() || self.select_folder(context, &folder) == 0 {
            warn!(context, "Cannot select folder '{}' for emptying", folder);
            return;
        }

        if !self.add_flag_finalized_with_set(context, SELECT_ALL, "\\Deleted") {
            warn!(context, "Cannot empty folder {}", folder);
        } else {
            if self.expunge_folder(context) {
                emit_event!(context, Event::ImapFolderEmptied(folder.to_string()));
            }
        }
    }
}

/// Try to get the folder meaning by the name of the folder only used if the server does not support XLIST.
// TODO: lots languages missing - maybe there is a list somewhere on other MUAs?
// however, if we fail to find out the sent-folder,
// only watching this folder is not working. at least, this is no show stopper.
// CAVE: if possible, take care not to add a name here that is "sent" in one language
// but sth. different in others - a hard job.
fn get_folder_meaning_by_name(folder_name: &imap::types::Name) -> FolderMeaning {
    let sent_names = vec!["sent", "sent objects", "gesendet"];
    let lower = folder_name.name().to_lowercase();

    if sent_names.into_iter().find(|s| *s == lower).is_some() {
        FolderMeaning::SentObjects
    } else {
        FolderMeaning::Unknown
    }
}

fn get_folder_meaning(folder_name: &imap::types::Name) -> FolderMeaning {
    if folder_name.attributes().is_empty() {
        return FolderMeaning::Unknown;
    }

    let mut res = FolderMeaning::Unknown;
    let special_names = vec!["\\Spam", "\\Trash", "\\Drafts", "\\Junk"];

    for attr in folder_name.attributes() {
        match attr {
            imap::types::NameAttribute::Custom(ref label) => {
                if special_names.iter().find(|s| *s == label).is_some() {
                    res = FolderMeaning::Other;
                } else if label == "\\Sent" {
                    res = FolderMeaning::SentObjects
                }
            }
            _ => {}
        }
    }

    match res {
        FolderMeaning::Unknown => get_folder_meaning_by_name(folder_name),
        _ => res,
    }
}

fn precheck_imf(context: &Context, rfc724_mid: &str, server_folder: &str, server_uid: u32) -> bool {
    if let Ok((old_server_folder, old_server_uid, msg_id)) =
        message::rfc724_mid_exists(context, &rfc724_mid)
    {
        if old_server_folder.is_empty() && old_server_uid == 0 {
            info!(context, "[move] detected bbc-self {}", rfc724_mid,);
            job_add(
                context,
                Action::MarkseenMsgOnImap,
                msg_id.to_u32() as i32,
                Params::new(),
                0,
            );
        } else if old_server_folder != server_folder {
            info!(context, "[move] detected moved message {}", rfc724_mid,);
            update_msg_move_state(context, &rfc724_mid, MoveState::Stay);
        }

        if old_server_folder != server_folder || old_server_uid != server_uid {
            update_server_uid(context, &rfc724_mid, server_folder, server_uid);
        }
        true
    } else {
        false
    }
}

fn prefetch_get_message_id(prefetch_msg: &imap::types::Fetch) -> Result<String, Error> {
    let message_id = prefetch_msg.envelope().unwrap().message_id.unwrap();
    wrapmime::parse_message_id(&message_id)
}
