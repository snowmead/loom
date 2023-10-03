#![feature(async_fn_in_trait)]
#![feature(async_closure)]

use std::{
	fmt::{Debug, Display},
	marker::PhantomData,
};

use async_openai::types::{ChatCompletionRequestMessage, ChatCompletionRequestMessageArgs, Role};
use serde::{Deserialize, Serialize};
use serenity::async_trait;
use storage::StorageError;
use tracing::{debug, error, instrument};

use crate::storage::Storage;

use self::models::{MaxTokens, Models};

mod storage;

pub trait Get<T> {
	fn get() -> T;
}

/// A trait representing a unique identifier for a weaving.
pub trait WeavingID: Debug + Display + Send + Sync + 'static {
	/// Returns the base key for a given [`WeavingID`].
	fn base_key(&self) -> String;
}

#[async_trait]
/// Specification for a storage handler.
///
/// The implementation of this trait is used to store and retrieve story parts.
pub trait StorageHandler<Key: WeavingID> {
	/// The error type for the storage handler.
	type Error: Display + Debug;

	/// Adds a [`StoryPart`] to storage for a given [`WeavingID`].
	async fn save_story_part(
		weaving_id: &Key,
		story_part: StoryPart,
		increment: bool,
	) -> Result<(), Self::Error>;
	/// Gets the last [`StoryPart`] from storage for a given [`WeavingID`].
	async fn get_last_story_part(weaving_id: &Key) -> Result<Option<StoryPart>, Self::Error>;
}

/// A trait consisting mainly of associated types implemented by [`Loreweaver`].
///
/// Normally structs implementing [`crate::Server`] would implement this trait to call methods
/// implemented by [`Loreweaver`]
#[async_trait]
/// A trait representing the configuration for a Loreweaver server.
pub trait Config {
	/// Getter for GPT model to use.
	type Model: Get<Models>;
	/// Type alias encompassing a server id and a story id.
	///
	/// Used mostly for querying some blob storage in the form of a path.
	type WeavingID: WeavingID;
}
/// An platform agnostic type representing a user's account ID.
pub type AccountId = u64;

/// Context message that represent a single message in a [`StoryPart`].
#[derive(Default, Clone, Debug, Serialize, Deserialize)]
pub struct ContextMessage {
	pub role: String,
	pub account_id: Option<String>,
	pub username: Option<String>,
	pub content: String,
	pub timestamp: String,
}

/// Represents a single part of a story containing a list of messages along with other metadata.
///
/// ChatGPT can only hold a limited amount of tokens in a the entire message history/context.
/// Therefore, at every [`Loom::prompt`] execution, we must keep track of the number of
/// `context_tokens` in the current story part and if it exceeds the maximum number of tokens
/// allowed for the current GPT [`Models`], then we must generate a summary of the current story
/// part and use that as the starting point for the next story part. This is one of the biggest
/// challenges for Loreweaver to keep a consistent narrative throughout the many story parts.
#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct StoryPart {
	/// Number of players that are part of the story. (typically this changes based on players
	/// entering the commands `/leave` or `/join`).
	///
	/// When generating a new story part (N + 1, where N is the current story part number), we need
	/// to copy over the same number of players. The story must remain consistent throughout each
	/// part.
	pub players: Vec<AccountId>,
	/// Total number of _GPT tokens_ in the story part.
	pub context_tokens: u16,
	/// List of [`ContextMessage`]s in the story part.
	pub context_messages: Vec<ContextMessage>,
}

/// A trait that defines all of the public associated methods that [`Loreweaver`] implements.
///
/// This is the machine that drives all of the core methods that should be used across any service
/// that needs to prompt ChatGPT and receive a response.
///
/// The implementations should handle all form of validation and usage tracking all while
/// abstracting the logic from the services calling them.
#[async_trait]
pub trait Loom<T: Config> {
	/// Prompt Loreweaver for a response for [`WeavingID`].
	///
	/// Prompts ChatGPT with the current [`StoryPart`] and the `msg`.
	///
	/// If 80% of the maximum number of tokens allowed in a message history for the configured
	/// ChatGPT [`Models`] has been reached, a summary will be generated instead of the current
	/// message history and saved to the cloud. A new message history will begin.
	async fn prompt(
		system: String,
		weaving_id: T::WeavingID,
		msg: String,
		account_id: AccountId,
		username: String,
		pseudo_username: Option<String>,
	) -> Result<String, WeaveError>;
}

/// The bread & butter of Loreweaver.
///
/// All core functionality is implemented by this struct.
pub struct Loreweaver<T: Config>(PhantomData<T>);

impl<T: Config> secret_lore::Sealed<T> for Loreweaver<T> {}

impl<T: Config> Loreweaver<T> {
	/// Maximum number of words to return in a response based on maximum tokens of GPT model or a
	/// `custom` supplied value.
	///
	/// Every token equates to 75% of a word.
	fn max_words(
		model: Models,
		custom_max_tokens: Option<MaxTokens>,
		context_tokens: MaxTokens,
	) -> MaxTokens {
		let max_tokens = custom_max_tokens
			.unwrap_or(Models::default_max_response_tokens(&model, context_tokens));

		(max_tokens as f64 * 0.75) as MaxTokens
	}
}

#[derive(Debug)]
pub enum WeaveError {
	/// Failed to prompt OpenAI.
	FailedPromptOpenAI,
	/// Failed to get content from OpenAI response.
	FailedToGetContent,
	/// A bad OpenAI role was supplied.
	BadOpenAIRole,
	/// Storage error.
	Storage(StorageError),
}

/// Wrapper around [`async_openai::types::types::Role`] for custom implementation.
enum WrapperRole {
	Role(Role),
}

impl From<WrapperRole> for Role {
	fn from(role: WrapperRole) -> Self {
		match role {
			WrapperRole::Role(role) => role,
		}
	}
}

impl From<String> for WrapperRole {
	fn from(role: String) -> Self {
		match role.as_str() {
			"system" => Self::Role(Role::System),
			"assistant" => Self::Role(Role::Assistant),
			"user" => Self::Role(Role::User),
			_ => panic!("Bad OpenAI role"),
		}
	}
}

#[async_trait]
impl<T: Config> Loom<T> for Loreweaver<T> {
	#[instrument]
	async fn prompt(
		system: String,
		weaving_id: T::WeavingID,
		msg: String,
		_account_id: AccountId,
		username: String,
		pseudo_username: Option<String>,
	) -> Result<String, WeaveError> {
		let model = T::Model::get();

		let mut story_part = Storage::get_last_story_part(&weaving_id)
			.await
			.map_err(|e| {
				error!("Failed to get last story part: {}", e);
				WeaveError::Storage(e)
			})?
			.unwrap_or_default();

		let username_with_nick = match pseudo_username {
			Some(pseudo_username) => format!("{}{}", username, pseudo_username),
			None => username,
		};

		story_part.context_messages.push(ContextMessage {
			role: "user".to_string(),
			account_id: None,
			username: Some(username_with_nick.clone()),
			content: msg.clone(),
			timestamp: chrono::Utc::now().to_rfc3339(),
		});

		// Add the system to the beginning of the message history
		let mut request_messages = vec![ChatCompletionRequestMessageArgs::default()
			.role(Role::System)
			.content(system)
			.build()
			.map_err(|e| {
				error!("Failed to build ChatCompletionRequestMessageArgs: {}", e);
				WeaveError::FailedPromptOpenAI
			})?
			.into()];

		request_messages.extend(
			story_part
				.context_messages
				.iter()
				.map(|msg: &ContextMessage| {
					ChatCompletionRequestMessageArgs::default()
						.content(msg.content.clone())
						.role(Into::<WrapperRole>::into(msg.role.clone()))
						.name(match msg.role.as_str() {
							"system" => "Loreweaver",
							"assistant" | "user" => username_with_nick.as_str(),
							_ => Err(WeaveError::BadOpenAIRole).unwrap(),
						})
						.build()
						.unwrap()
				})
				.collect::<Vec<ChatCompletionRequestMessage>>(),
		);

		let max_response_words =
			Loreweaver::<T>::max_words(model, None, story_part.context_tokens as u128);

		let res = <Loreweaver<T> as secret_lore::Sealed<T>>::do_prompt(
			T::Model::get(),
			&mut request_messages,
			max_response_words,
		)
		.await
		.map_err(|e| {
			error!("Failed to prompt ChatGPT: {}", e);
			WeaveError::FailedPromptOpenAI
		})?;

		let response_content =
			res.choices[0].clone().message.content.ok_or(WeaveError::FailedToGetContent)?;

		story_part.context_messages.push(ContextMessage {
			role: "assistant".to_string(),
			account_id: None,
			username: None,
			content: response_content.clone(),
			timestamp: chrono::Utc::now().to_rfc3339(),
		});

		debug!("Saving story part: {:?}", story_part.context_messages);

		Storage::save_story_part(&weaving_id, story_part, false).await.map_err(|e| {
			error!("Failed to save story part: {}", e);
			WeaveError::Storage(e)
		})?;

		Ok(response_content)
	}
}

pub mod models {
	use clap::{builder::PossibleValue, ValueEnum};

	pub type MaxTokens = u128;

	/// The ChatGPT language models that are available to use.
	#[derive(PartialEq, Eq, Clone, Debug, Copy)]
	pub enum Models {
		GPT3,
		GPT4,
	}

	/// Clap value enum implementation for argument parsing.
	impl ValueEnum for Models {
		fn value_variants<'a>() -> &'a [Self] {
			&[Self::GPT3, Self::GPT4]
		}

		fn to_possible_value(&self) -> Option<PossibleValue> {
			Some(match self {
				Self::GPT3 => PossibleValue::new(Self::GPT3.name()),
				Self::GPT4 => PossibleValue::new(Self::GPT4.name()),
			})
		}
	}

	impl Models {
		/// Get the model name.
		pub fn name(&self) -> &'static str {
			match self {
				Self::GPT3 => "gpt-3.5-turbo",
				Self::GPT4 => "gpt-4",
			}
		}

		/// Default maximum tokens to respond with for a ChatGPT prompt.
		///
		/// This would normally be used when prompting ChatGPT API and specifying the maximum tokens
		/// to return.
		///
		/// `tokens_in_context` parameter is the current number of tokens that are part of the
		/// context. This should not surpass the [`max_context_tokens`]
		pub fn default_max_response_tokens(
			model: &Models,
			tokens_in_context: MaxTokens,
		) -> MaxTokens {
			(model.max_context_tokens() - tokens_in_context) / 3
		}

		/// Maximum number of tokens that can be processed at once by ChatGPT.
		pub fn max_context_tokens(&self) -> MaxTokens {
			match self {
				Self::GPT3 => 4_096,
				Self::GPT4 => 8_192,
			}
		}
	}
}

mod secret_lore {
	use async_openai::{
		config::OpenAIConfig,
		error::OpenAIError,
		types::{
			ChatCompletionRequestMessage, ChatCompletionRequestMessageArgs,
			CreateChatCompletionRequestArgs, CreateChatCompletionResponse, Role,
		},
	};
	use lazy_static::lazy_static;
	use tiktoken_rs::p50k_base;
	use tokio::sync::RwLock;
	use tracing::error;

	use super::{
		models::{MaxTokens, Models},
		Config,
	};

	lazy_static! {
		/// The OpenAI client to interact with the OpenAI API.
		static ref OPENAI_CLIENT: RwLock<async_openai::Client<OpenAIConfig>> = RwLock::new(async_openai::Client::new());
	}

	pub trait Sealed<T: Config> {
		/// The action to query ChatGPT with the supplied configurations and messages.
		///
		/// Auto injects a system message at the end of vec of messages to instruct ChatGPT to
		/// respond with a certain number of words.
		///
		/// We do this here to avoid any other service from having to do this.
		async fn do_prompt(
			model: Models,
			msgs: &mut Vec<ChatCompletionRequestMessage>,
			max_words: MaxTokens,
		) -> Result<CreateChatCompletionResponse, OpenAIError> {
			msgs.push(
				ChatCompletionRequestMessageArgs::default()
					.content(format!("Respond with {} words or less", max_words))
					.role(Role::System)
					.build()
					.map_err(|e| {
						error!("Failed to build ChatCompletionRequestMessageArgs: {}", e);
						e
					})?
					.into(),
			);

			let request = CreateChatCompletionRequestArgs::default()
				.max_tokens(300u16)
				.temperature(0.9f32)
				.presence_penalty(0.6f32)
				.frequency_penalty(0.6f32)
				.model(model.name())
				// .suffix("Loreweaver:")
				.messages(msgs.to_owned())
				.build()?;

			OPENAI_CLIENT.read().await.chat().create(request).await
		}
	}

	/// Tokens are a ChatGPT concept which represent normally a third of a word (or 75%).
	///
	/// This trait auto implements some basic utility methods for counting the number of tokens from
	/// a string.
	pub trait Tokens: ToString {
		/// Count the number of tokens in the string.
		fn count_tokens(&self) -> MaxTokens {
			let bpe = p50k_base().unwrap();
			let tokens = bpe.encode_with_special_tokens(&self.to_string());

			tokens.len() as MaxTokens
		}
	}

	/// Implement the trait for String.
	///
	/// This is done so that we can call `count_tokens` on a String.
	impl Tokens for String {}
}