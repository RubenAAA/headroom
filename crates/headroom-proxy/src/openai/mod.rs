//! Translating between the Anthropic Messages wire format and OpenAI's two.
//!
//! Split out of `handlers::local_model` on 2026-08-26. The name it lived under
//! was misleading: none of this is about a local model. It is what every
//! `:translate` route runs, and the busiest user of it is Codex.
//!
//! The split follows the direction of travel. `request` goes out to OpenAI,
//! `response` and `stream` come back — buffered and streaming respectively,
//! because the streaming case needs a state machine and the buffered one does
//! not.
pub(crate) mod request;
pub(crate) mod response;
pub(crate) mod stream;
