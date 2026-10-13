//! One WebSocket to a connection manager, and the two ways of using it.
//!
//! **Asking.** Most of what a client does is a question: who is this, may I
//! sign in, send this. Each is a packet that names a job, and the answer is a
//! packet that names the same job; [`Link::call`] sends one and waits for its
//! answer, however many others arrive in between.
//!
//! **Being told.** The rest is things the server says unprompted - a friend
//! came online, a message arrived, you were signed out. Those come out of the
//! channel [`open`] returns, in the order they arrived, with the batches
//! already taken apart.
//!
//! A `Link` is a cheap handle: cloning it gives another way to ask, and the
//! connection closes when the last one is dropped.

use super::eresult;
use super::proto::Header;
use super::wire::{self, emsg, Packet, NO_JOB};
use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use prost::Message;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message as Ws;

/// How long to wait for an answer before saying there is none.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// Who this connection is signed in as: stamped on everything sent once it is.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Identity {
    pub steamid: u64,
    pub session: i32,
}

/// Why a question got no answer, or a bad one.
#[derive(Debug)]
pub enum CallError {
    /// Steam answered, and the answer was no. The code and, where Steam said,
    /// its own words.
    Refused(i32, Option<String>),
    /// The connection ended before the answer came.
    Closed,
    /// No answer arrived in time.
    TimedOut,
    /// An answer arrived that could not be read.
    Unreadable(String),
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallError::Refused(code, said) => {
                let words = eresult::describe(*code);
                // Steam's own words go after ours when they add anything.
                match said.as_deref().filter(|m| !m.eq_ignore_ascii_case(&words)) {
                    Some(said) => write!(f, "{words} ({said})"),
                    None => write!(f, "{words}"),
                }
            }
            CallError::Closed => write!(f, "the connection to Steam ended"),
            CallError::TimedOut => write!(f, "Steam did not answer in time"),
            CallError::Unreadable(why) => {
                write!(f, "an answer from Steam could not be read: {why}")
            }
        }
    }
}

impl std::error::Error for CallError {}

/// The questions waiting for an answer, and whether there is still a
/// connection to answer them.
///
/// The two are one thing behind one lock so that a question cannot be added
/// after the connection ended and before anybody noticed: that question would
/// wait out its whole timeout for an answer that cannot come.
#[derive(Default)]
struct Waiting {
    ended: bool,
    by_job: HashMap<u64, oneshot::Sender<Packet>>,
}

type Jobs = Arc<Mutex<Waiting>>;

struct Inner {
    out: mpsc::UnboundedSender<Vec<u8>>,
    jobs: Jobs,
    next_job: AtomicU64,
    identity: Mutex<Identity>,
    timeout: Duration,
    tasks: [tokio::task::AbortHandle; 2],
}

impl Drop for Inner {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

#[derive(Clone)]
pub struct Link(Arc<Inner>);

/// Opens the connection, routed as `key` is, and starts reading it.
pub async fn open(key: &str, url: &str) -> Result<(Link, mpsc::UnboundedReceiver<Packet>)> {
    open_with(key, url, CALL_TIMEOUT).await
}

pub async fn open_with(
    key: &str,
    url: &str,
    timeout: Duration,
) -> Result<(Link, mpsc::UnboundedReceiver<Packet>)> {
    let (socket, _) = crate::net::route::websocket(key, url)
        .await
        .with_context(|| format!("connecting to {url}"))?;
    let (mut sink, mut stream) = socket.split();
    let (out, mut out_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let (told, told_rx) = mpsc::unbounded_channel::<Packet>();
    let jobs: Jobs = Arc::new(Mutex::new(Waiting::default()));

    let writer = tokio::spawn(async move {
        while let Some(bytes) = out_rx.recv().await {
            if sink.send(Ws::Binary(bytes)).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    });

    let reader = tokio::spawn({
        let jobs = jobs.clone();
        let key = key.to_string();
        async move {
            let mut why = "the stream ended".to_string();
            loop {
                let frame = match stream.next().await {
                    None => break,
                    Some(Err(e)) => {
                        why = format!("a read failed: {e}");
                        break;
                    }
                    Some(Ok(frame)) => frame,
                };
                let bytes = match frame {
                    Ws::Binary(bytes) => bytes,
                    Ws::Close(close) => {
                        why = format!(
                            "Steam closed it: {}",
                            close
                                .map(|c| format!("{} {}", c.code, c.reason))
                                .unwrap_or_else(|| "no reason given".to_string())
                        );
                        break;
                    }
                    // Pings are answered by the library; text is not a thing
                    // this server sends.
                    _ => continue,
                };
                let packet = match Packet::decode(&bytes) {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::debug!("steam: a packet that could not be read: {e:#}");
                        continue;
                    }
                };
                let packets = if packet.emsg == emsg::MULTI {
                    match wire::unpack_multi(&packet.body) {
                        Ok(inside) => inside,
                        Err(e) => {
                            tracing::debug!("steam: a batch that could not be read: {e:#}");
                            continue;
                        }
                    }
                } else {
                    vec![packet]
                };
                for packet in packets {
                    // An answer goes to whoever asked; anything else is news.
                    let waiting = match packet.job_target() {
                        NO_JOB => None,
                        job => jobs.lock().unwrap().by_job.remove(&job),
                    };
                    match waiting {
                        Some(waiter) => {
                            let _ = waiter.send(packet);
                        }
                        None => {
                            if told.send(packet).is_err() {
                                return;
                            }
                        }
                    }
                }
            }
            tracing::info!("steam[{key}]: the connection ended: {why}");
            // Nobody is going to be answered now. Dropping the waiters is how
            // each of them finds out, and dropping `told` is how the owner of
            // the connection does. Marked ended in the same breath, so a
            // question asked from here on fails at once.
            let mut waiting = jobs.lock().unwrap();
            waiting.ended = true;
            waiting.by_job.clear();
        }
    });

    let link = Link(Arc::new(Inner {
        out,
        jobs,
        next_job: AtomicU64::new(1),
        identity: Mutex::new(Identity::default()),
        timeout,
        tasks: [writer.abort_handle(), reader.abort_handle()],
    }));
    Ok((link, told_rx))
}

impl Link {
    /// Who this connection says it is from now on.
    pub fn set_identity(&self, identity: Identity) {
        *self.0.identity.lock().unwrap() = identity;
    }

    pub fn identity(&self) -> Identity {
        *self.0.identity.lock().unwrap()
    }

    fn header(&self, signed_in: bool) -> Header {
        let id = self.identity();
        Header {
            steamid: (signed_in && id.steamid != 0).then_some(id.steamid),
            client_sessionid: (id.session != 0).then_some(id.session),
            ..Header::default()
        }
    }

    /// Whether the connection is still there to send on.
    pub fn is_open(&self) -> bool {
        !self.0.out.is_closed() && !self.0.jobs.lock().unwrap().ended
    }

    /// Sends a message that expects no answer.
    pub fn send<M: Message>(&self, emsg: u32, message: &M) -> Result<(), CallError> {
        let packet = Packet::new(emsg, self.header(true), message.encode_to_vec());
        self.0
            .out
            .send(packet.encode())
            .map_err(|_| CallError::Closed)
    }

    /// A service method that is said and not answered: Steam's `SendNotification`,
    /// as against a call. Nothing waits for anything.
    pub fn notify<Q: Message>(&self, method: &str, request: &Q) -> Result<(), CallError> {
        let mut header = self.header(true);
        header.target_job_name = Some(method.to_string());
        header.realm = Some(1);
        let packet = Packet::new(
            emsg::SERVICE_METHOD_CALL_FROM_CLIENT,
            header,
            request.encode_to_vec(),
        );
        self.0
            .out
            .send(packet.encode())
            .map_err(|_| CallError::Closed)
    }

    /// A question to Steam in one of its older messages, and the answer.
    ///
    /// These are not service methods: there is no method name, and the
    /// result code is inside the answer rather than in its header, so it is
    /// the caller's to read. What matches the answer to the question is the
    /// same job id.
    pub async fn ask<Q: Message, A: Message + Default>(
        &self,
        emsg: u32,
        request: &Q,
    ) -> Result<A, CallError> {
        let job = self.0.next_job.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            let mut waiting = self.0.jobs.lock().unwrap();
            if waiting.ended {
                return Err(CallError::Closed);
            }
            waiting.by_job.insert(job, tx);
        }
        let mut header = self.header(true);
        header.jobid_source = Some(job);
        let packet = Packet::new(emsg, header, request.encode_to_vec());
        if self.0.out.send(packet.encode()).is_err() {
            self.0.jobs.lock().unwrap().by_job.remove(&job);
            return Err(CallError::Closed);
        }
        let answer = match tokio::time::timeout(self.0.timeout, rx).await {
            Ok(Ok(answer)) => answer,
            Ok(Err(_)) => return Err(CallError::Closed),
            Err(_) => {
                self.0.jobs.lock().unwrap().by_job.remove(&job);
                return Err(CallError::TimedOut);
            }
        };
        A::decode(answer.body.as_slice()).map_err(|e| CallError::Unreadable(e.to_string()))
    }

    /// A question to one of Steam's services, and its answer.
    ///
    /// `method` is the service and method as Steam names them, `Service.Method#1`.
    /// `signed_in` says whether to speak as the account: the calls that make
    /// signing in possible are made before there is one.
    pub async fn call<Q: Message, A: Message + Default>(
        &self,
        method: &str,
        request: &Q,
        signed_in: bool,
    ) -> Result<A, CallError> {
        let job = self.0.next_job.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            let mut waiting = self.0.jobs.lock().unwrap();
            if waiting.ended {
                return Err(CallError::Closed);
            }
            waiting.by_job.insert(job, tx);
        }

        let mut header = self.header(signed_in);
        header.jobid_source = Some(job);
        header.target_job_name = Some(method.to_string());
        header.realm = Some(1);
        let kind = if signed_in {
            emsg::SERVICE_METHOD_CALL_FROM_CLIENT
        } else {
            emsg::SERVICE_METHOD_CALL_FROM_CLIENT_NON_AUTHED
        };
        let packet = Packet::new(kind, header, request.encode_to_vec());
        if self.0.out.send(packet.encode()).is_err() {
            self.0.jobs.lock().unwrap().by_job.remove(&job);
            return Err(CallError::Closed);
        }

        let answer = match tokio::time::timeout(self.0.timeout, rx).await {
            Ok(Ok(answer)) => answer,
            Ok(Err(_)) => return Err(CallError::Closed),
            Err(_) => {
                self.0.jobs.lock().unwrap().by_job.remove(&job);
                return Err(CallError::TimedOut);
            }
        };
        let code = answer.eresult();
        if code != eresult::OK {
            // Steam asking for its agreement to be accepted puts the page for it in the answer, and
            // that page is the whole of what can be done about it.
            let said = answer
                .header
                .error_message
                .filter(|m| !m.is_empty())
                .or_else(|| {
                    (code == eresult::MUST_AGREE_TO_SSA)
                        .then(|| agreement_page(&answer.body))
                        .flatten()
                });
            return Err(CallError::Refused(code, said));
        }
        A::decode(answer.body.as_slice()).map_err(|e| CallError::Unreadable(e.to_string()))
    }
}

/// The address Steam gives for the agreement it wants accepted: the sign-in answers carry it as
/// field 7 and the polling answer as field 8.
fn agreement_page(body: &[u8]) -> Option<String> {
    #[derive(Message)]
    struct Page {
        #[prost(string, optional, tag = "7")]
        sign_in: Option<String>,
        #[prost(string, optional, tag = "8")]
        poll: Option<String>,
    }
    let page = Page::decode(body).ok()?;
    page.sign_in
        .or(page.poll)
        .filter(|u| u.starts_with("https://"))
}

#[cfg(test)]
mod agreement_tests {
    use super::*;

    #[test]
    fn the_page_for_the_agreement_steam_wants_is_found_in_either_kind_of_answer() {
        #[derive(Message)]
        struct Begin {
            #[prost(string, optional, tag = "7")]
            url: Option<String>,
        }
        #[derive(Message)]
        struct Poll {
            #[prost(string, optional, tag = "8")]
            url: Option<String>,
        }
        assert_eq!(
            agreement_page(
                &Begin {
                    url: Some("https://login.steampowered.com/a".into())
                }
                .encode_to_vec()
            )
            .as_deref(),
            Some("https://login.steampowered.com/a")
        );
        assert_eq!(
            agreement_page(
                &Poll {
                    url: Some("https://login.steampowered.com/b".into())
                }
                .encode_to_vec()
            )
            .as_deref(),
            Some("https://login.steampowered.com/b")
        );
        assert_eq!(
            agreement_page(
                &Poll {
                    url: Some("javascript:alert(1)".into())
                }
                .encode_to_vec()
            ),
            None,
            "only an https address is passed on"
        );
        assert_eq!(agreement_page(&[0xff, 0xff]), None);
    }
}

/// A connection manager that is not one: speaks the framing, over a plain
/// WebSocket on the loopback, as a script says. For the tests of everything
/// built on a connection.
#[cfg(test)]
pub(crate) mod fake {
    use super::*;
    use tokio::net::TcpListener;

    /// What a scripted server does with each packet it is sent: the packets to
    /// send back, in order. Returning `None` closes the connection.
    pub type Script = Box<dyn FnMut(Packet) -> Option<Vec<Packet>> + Send>;

    /// A server on a free port; the `ws://` address to open.
    pub async fn serve(mut script: Script) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            while let Some(Ok(frame)) = ws.next().await {
                let Ws::Binary(bytes) = frame else { continue };
                let packet = Packet::decode(&bytes).expect("a packet from the client");
                match script(packet) {
                    Some(replies) => {
                        for reply in replies {
                            if ws.send(Ws::Binary(reply.encode())).await.is_err() {
                                return;
                            }
                        }
                    }
                    None => {
                        let _ = ws.close(None).await;
                        return;
                    }
                }
            }
        });
        format!("ws://{addr}")
    }

    /// The answer to a call: the request's job as its target, a result, a body.
    pub fn answer(request: &Packet, code: i32, body: Vec<u8>) -> Packet {
        Packet::new(
            emsg::SERVICE_METHOD_RESPONSE,
            Header {
                jobid_target: request.header.jobid_source,
                eresult: Some(code),
                ..Header::default()
            },
            body,
        )
    }

    /// Something the server says unprompted.
    pub fn tell(emsg: u32, body: Vec<u8>) -> Packet {
        Packet::new(emsg, Header::default(), body)
    }
}

#[cfg(test)]
mod tests {
    use super::fake::*;
    use super::*;
    use crate::backend::steam::proto::*;

    const KEY: &str = "steam:test";

    #[tokio::test]
    async fn a_question_gets_its_own_answer_whatever_arrives_between() {
        let url = serve(Box::new(|req| {
            assert_eq!(
                req.header.target_job_name.as_deref(),
                Some("Authentication.GetPasswordRSAPublicKey#1")
            );
            let ask = GetPasswordRsaPublicKeyRequest::decode(req.body.as_slice()).unwrap();
            assert_eq!(ask.account_name.as_deref(), Some("someone"));
            // News first, to be sure the answer is matched by its job and not
            // by being the next thing to arrive.
            Some(vec![
                tell(
                    emsg::CLIENT_ACCOUNT_INFO,
                    ClientAccountInfo {
                        persona_name: Some("Some One".into()),
                        ..Default::default()
                    }
                    .encode_to_vec(),
                ),
                answer(
                    &req,
                    1,
                    GetPasswordRsaPublicKeyResponse {
                        publickey_mod: Some("AB".into()),
                        publickey_exp: Some("010001".into()),
                        timestamp: Some(7),
                    }
                    .encode_to_vec(),
                ),
            ])
        }))
        .await;
        let (link, mut told) = open(KEY, &url).await.unwrap();
        let got: GetPasswordRsaPublicKeyResponse = link
            .call(
                "Authentication.GetPasswordRSAPublicKey#1",
                &GetPasswordRsaPublicKeyRequest {
                    account_name: Some("someone".into()),
                },
                false,
            )
            .await
            .unwrap();
        assert_eq!(
            (got.publickey_mod.as_deref(), got.timestamp),
            (Some("AB"), Some(7))
        );
        let news = told.recv().await.unwrap();
        assert_eq!(news.emsg, emsg::CLIENT_ACCOUNT_INFO);
    }

    #[tokio::test]
    async fn the_question_says_who_is_asking_only_once_there_is_somebody() {
        let seen = Arc::new(Mutex::new(Vec::<Packet>::new()));
        let url = serve({
            let seen = seen.clone();
            Box::new(move |req| {
                seen.lock().unwrap().push(req.clone());
                Some(vec![answer(&req, 1, Vec::new())])
            })
        })
        .await;
        let (link, _told) = open(KEY, &url).await.unwrap();
        let _: ClientLogOff = link.call("A.B#1", &ClientLogOff {}, false).await.unwrap();
        link.set_identity(Identity {
            steamid: 76561197960287930,
            session: 55,
        });
        let _: ClientLogOff = link.call("A.B#1", &ClientLogOff {}, true).await.unwrap();

        let seen = seen.lock().unwrap();
        assert_eq!(
            seen[0].emsg,
            emsg::SERVICE_METHOD_CALL_FROM_CLIENT_NON_AUTHED
        );
        assert_eq!(seen[0].header.steamid, None);
        assert_eq!(seen[0].header.realm, Some(1));
        assert_eq!(seen[1].emsg, emsg::SERVICE_METHOD_CALL_FROM_CLIENT);
        assert_eq!(seen[1].header.steamid, Some(76561197960287930));
        assert_eq!(seen[1].header.client_sessionid, Some(55));
        assert_ne!(
            seen[0].header.jobid_source, seen[1].header.jobid_source,
            "each question is its own job"
        );
    }

    #[tokio::test]
    async fn a_no_is_an_error_carrying_the_code_and_steams_own_words() {
        let url = serve(Box::new(|req| {
            let mut no = answer(&req, eresult::RATE_LIMIT_EXCEEDED, Vec::new());
            no.header.error_message = Some("slow down".into());
            Some(vec![no])
        }))
        .await;
        let (link, _told) = open(KEY, &url).await.unwrap();
        let err = link
            .call::<_, ClientLogOff>("A.B#1", &ClientLogOff {}, false)
            .await
            .unwrap_err();
        assert!(matches!(&err, CallError::Refused(84, Some(m)) if m == "slow down"));
        assert!(err.to_string().contains("limiting"));
        assert!(
            err.to_string().contains("(slow down)"),
            "Steam's own words are kept: {err}"
        );
    }

    #[tokio::test]
    async fn a_batch_is_taken_apart_and_its_news_arrives_in_order() {
        let url = serve(Box::new(|_| {
            let inside = [
                tell(
                    emsg::CLIENT_FRIENDS_LIST,
                    ClientFriendsList {
                        bincremental: Some(false),
                        ..Default::default()
                    }
                    .encode_to_vec(),
                ),
                tell(
                    emsg::CLIENT_PERSONA_STATE,
                    ClientPersonaState::default().encode_to_vec(),
                ),
            ];
            Some(vec![tell(emsg::MULTI, wire::pack_multi(&inside, true))])
        }))
        .await;
        let (link, mut told) = open(KEY, &url).await.unwrap();
        link.send(emsg::CLIENT_HEART_BEAT, &ClientHeartBeat::default())
            .unwrap();
        assert_eq!(told.recv().await.unwrap().emsg, emsg::CLIENT_FRIENDS_LIST);
        assert_eq!(told.recv().await.unwrap().emsg, emsg::CLIENT_PERSONA_STATE);
    }

    #[tokio::test]
    async fn a_connection_that_ends_fails_the_questions_waiting_and_closes_the_news() {
        let url = serve(Box::new(|_| None)).await;
        let (link, mut told) = open(KEY, &url).await.unwrap();
        let err = link
            .call::<_, ClientLogOff>("A.B#1", &ClientLogOff {}, false)
            .await
            .unwrap_err();
        assert!(matches!(err, CallError::Closed), "{err:?}");
        assert!(
            told.recv().await.is_none(),
            "the news stops when the connection does"
        );
        // And asking after it has gone fails at once rather than waiting.
        let err = link
            .call::<_, ClientLogOff>("A.B#1", &ClientLogOff {}, false)
            .await
            .unwrap_err();
        assert!(matches!(err, CallError::Closed), "{err:?}");
    }

    #[tokio::test]
    async fn no_answer_is_a_timeout_and_the_job_is_forgotten() {
        let url = serve(Box::new(|_| Some(Vec::new()))).await;
        let (link, _told) = open_with(KEY, &url, Duration::from_millis(150))
            .await
            .unwrap();
        let err = link
            .call::<_, ClientLogOff>("A.B#1", &ClientLogOff {}, false)
            .await
            .unwrap_err();
        assert!(matches!(err, CallError::TimedOut), "{err:?}");
        assert!(
            link.0.jobs.lock().unwrap().by_job.is_empty(),
            "a question nobody is waiting on is not kept"
        );
    }

    #[tokio::test]
    async fn an_answer_that_is_not_what_was_asked_for_is_unreadable_not_a_panic() {
        let url = serve(Box::new(|req| {
            Some(vec![answer(&req, 1, vec![0xff, 0xff, 0xff])])
        }))
        .await;
        let (link, _told) = open(KEY, &url).await.unwrap();
        let err = link
            .call::<_, ClientFriendsList>("A.B#1", &ClientLogOff {}, false)
            .await
            .unwrap_err();
        assert!(matches!(err, CallError::Unreadable(_)), "{err:?}");
    }

    #[tokio::test]
    async fn dropping_the_last_handle_closes_the_connection() {
        let closed = Arc::new(tokio::sync::Notify::new());
        let url = {
            let closed = closed.clone();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                let (tcp, _) = listener.accept().await.unwrap();
                let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
                while ws.next().await.is_some() {}
                closed.notify_one();
            });
            format!("ws://{addr}")
        };
        let (link, told) = open(KEY, &url).await.unwrap();
        let other = link.clone();
        drop(link);
        assert!(other.is_open());
        drop(other);
        drop(told);
        tokio::time::timeout(Duration::from_secs(5), closed.notified())
            .await
            .expect("the server saw the connection close");
    }
}
