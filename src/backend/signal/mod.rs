//! Signal, as a secondary device linked to an account that lives on somebody's phone.
//!
//! The protocol itself - the ratchets, sealed sender, groups, the storage service - is Signal's own `libsignal`, reached
//! through Whisperfish's `libsignal-service-rs` and `presage` (all AGPL-3.0, see Cargo.toml for why that is accepted and
//! how the dependency graph was made to resolve). This module is what moho adds around them: where the account's keys
//! and conversations are kept (`store`), the sign-in by QR code (`link`), and, to come, the connection and the translation into
//! the daemon's unified model.

// The store is built before the connection that uses it (issue 35, Round 0/2); each piece is used as it lands.
#![allow(dead_code)]

pub mod backup;
pub mod commands;
pub mod compose;
pub mod connect;
pub mod expiry;
pub mod groups;
pub mod ids;
pub mod inbox;
pub mod link;
pub mod manage;
pub mod local;
pub mod media;
pub mod memory;
pub mod network;
pub mod people;
pub mod polls;
pub mod profile;
pub mod ranges;
pub mod requests;
pub mod revisions;
pub mod send;
pub mod storage;
pub mod store;
pub mod stories;
pub mod unlink;
pub mod viewonce;

pub use connect::spawn;

/// What the rest of the daemon needs to act on a connected account. The library's manager lives on the account's own
/// thread (see `local`), so the daemon asks that thread to act by sending it a `Command`.
#[derive(Clone)]
pub struct Handle {
    pub account_id: String,
    /// The account's own ACI.
    pub own: uuid::Uuid,
    /// The account's store, for looking names up from outside its thread.
    pub store: store::SignalStore,
    pub memory: std::sync::Arc<memory::Memory>,
    pub commands: tokio::sync::mpsc::UnboundedSender<Command>,
}

/// One of the account's devices, as the phone's list of linked devices shows it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DeviceRow {
    pub id: u8,
    pub name: String,
    #[serde(rename = "createdAt")]
    pub created_at: i64,
    #[serde(rename = "lastSeen")]
    pub last_seen: i64,
    #[serde(rename = "isThis")]
    pub is_this: bool,
}

/// A file to put on Signal's servers.
#[derive(Debug)]
pub struct Upload {
    pub name: String,
    pub content_type: String,
    pub bytes: Vec<u8>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Spoken into the window (a voice message), not a file of audio.
    pub voice: bool,
}

/// How a command says how it went: the time the message was sent, or why it was not.
pub type Done = tokio::sync::oneshot::Sender<Result<u64, String>>;

/// Something the daemon asks the account's thread to do.
#[derive(Debug)]
pub enum Command {
    /// Ask the phone to send its contacts again.
    RefreshContacts,
    /// Say something. `body` is plain text and `ranges` the styles over it; `view_once` makes its one picture or video
    /// something each person can open once.
    Send { thread: presage::store::Thread, body: String, ranges: Vec<presage::proto::BodyRange>, quote: Option<presage::proto::data_message::Quote>, attachments: Vec<presage::proto::AttachmentPointer>, view_once: bool, done: Done },
    /// Encrypt files and put them on Signal's file servers; the answer is the pointers a message carries.
    Upload { files: Vec<Upload>, done: tokio::sync::oneshot::Sender<Result<Vec<presage::proto::AttachmentPointer>, String>> },
    /// React to a message, or take the reaction back.
    React { thread: presage::store::Thread, target_ms: u64, target_author: uuid::Uuid, emoji: String, remove: bool, done: Done },
    /// Tell somebody their messages were delivered, read or viewed.
    Receipt { to: presage::libsignal_service::protocol::ServiceId, kind: presage::proto::receipt_message::Type, timestamps: Vec<u64> },
    /// List the account's devices.
    Devices { done: tokio::sync::oneshot::Sender<Result<Vec<DeviceRow>, String>> },
    /// Change the name this device has in the phone's list.
    Rename { name: String, done: tokio::sync::oneshot::Sender<Result<(), String>> },
    /// Fetch a person's profile (name and picture), once their profile key is known.
    Profile { who: uuid::Uuid },
    /// Download and decrypt an attachment.
    Fetch { pointer: Box<presage::proto::AttachmentPointer>, done: tokio::sync::oneshot::Sender<Result<Vec<u8>, String>> },
    /// Tell the account's other devices these messages were read here.
    ReadSync { reads: Vec<(uuid::Uuid, u64)> },
    /// Say the account is typing in a conversation, or has stopped.
    Typing { thread: presage::store::Thread, started: bool },
    /// Ask the phone to send something again: its settings, its contacts.
    AskPhone(presage::proto::sync_message::request::Type),
    /// Take one of the account's own messages back for everyone.
    Delete { thread: presage::store::Thread, target_ms: u64, done: Done },
    /// Find who has a username.
    LookUpUsername { name: String, done: tokio::sync::oneshot::Sender<Result<Option<uuid::Uuid>, String>> },
    /// Find who has a number (E.164), through Signal's contact discovery.
    LookUpNumber { number: String, done: tokio::sync::oneshot::Sender<Result<Option<uuid::Uuid>, String>> },
    /// A change to a group; `picture` is uploaded first, for an `Edit::Picture`, which then names it.
    EditGroup { key: [u8; 32], edit: manage::Edit, picture: Option<Vec<u8>>, done: tokio::sync::oneshot::Sender<Result<(), String>> },
    /// Make a group; the answer is its master key.
    CreateGroup { title: String, members: Vec<uuid::Uuid>, done: tokio::sync::oneshot::Sender<Result<[u8; 32], String>> },
    /// What a group link leads to: (title, members, whether an admin approves).
    LinkInfo { link: String, done: tokio::sync::oneshot::Sender<Result<(String, u32, bool), String>> },
    /// Join a group through its link; the answer is whether the account is in (false: waiting for an admin).
    JoinByLink { link: String, done: tokio::sync::oneshot::Sender<Result<([u8; 32], bool), String>> },
    /// Set the disappearing-message timer of a conversation with one person.
    SetTimer { thread: presage::store::Thread, seconds: u32, done: Done },
    /// Block or unblock a person or group: the account's other devices are sent the new list.
    Block { thread: presage::store::Thread, blocked: bool, done: tokio::sync::oneshot::Sender<Result<(), String>> },
    /// Mark a person's safety number verified (or take that back), on every device of the account.
    Verify { who: uuid::Uuid, verified: bool, done: tokio::sync::oneshot::Sender<Result<(), String>> },
    /// Change the account's own profile (the name and about others see); the phone is told to fetch it. `picture` is left
    /// alone when `None`, and replaced (`Some(Some)`) or taken away (`Some(None)`) otherwise.
    SetProfile { given: String, family: String, about: String, emoji: String, picture: Option<Option<Vec<u8>>>, done: tokio::sync::oneshot::Sender<Result<(), String>> },
    /// Vote in a poll (a DataMessage carrying the vote).
    Vote { thread: presage::store::Thread, vote: presage::proto::data_message::PollVote, done: Done },
    /// The end of a poll the account asked, sent at `target_ms`.
    EndPoll { thread: presage::store::Thread, target_ms: u64, done: Done },
    /// A new poll, asked in the conversation.
    Poll { thread: presage::store::Thread, create: presage::proto::data_message::PollCreate, done: Done },
    /// Answer a story: a reply in words or a reaction, sent to its author naming the story.
    StoryAnswer { thread: presage::store::Thread, context: presage::proto::data_message::StoryContext, body: Option<String>, emoji: Option<(String, bool)>, done: Done },
    /// Send a sticker from one of the account's installed packs.
    SendSticker { thread: presage::store::Thread, pack_id: Vec<u8>, sticker_id: u32, done: Done },
    /// Read the storage service again, if it changed.
    Storage,
    /// Answer a message request: tell the account's other devices, and the person when it is accepted.
    AnswerRequest { thread: presage::store::Thread, answer: requests::Answer, done: tokio::sync::oneshot::Sender<Result<(), String>> },
    /// Post a story to people (My Story or a custom list, named by its id for the account's other devices).
    PostStoryTo { people: Vec<uuid::Uuid>, list: uuid::Uuid, story: Box<presage::proto::StoryMessage>, done: Done },
    /// Post a story to a group: its members see it for a day.
    PostStory { key: [u8; 32], story: Box<presage::proto::StoryMessage>, done: Done },
    /// Stories looked at here: the other devices are told, and each author when `receipt`.
    StoriesViewed { views: Vec<(uuid::Uuid, u64)>, receipt: bool },
    /// A view-once message was opened here: the other devices are told, and the sender if `receipt`.
    ViewOnceOpened { sender: uuid::Uuid, sent_ms: u64, receipt: bool },
    /// Change one of the account's own messages.
    Edit { thread: presage::store::Thread, target_ms: u64, body: String, ranges: Vec<presage::proto::BodyRange>, done: Done },
}
