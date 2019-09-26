use super::prelude::*;
use dawn_model::{channel::Webhook, id::ChannelId};

#[derive(Serialize)]
pub struct CreateWebhook<'a> {
    avatar: Option<String>,
    name: String,
    #[serde(skip)]
    channel_id: ChannelId,
    #[serde(skip)]
    fut: Option<Pin<Box<dyn Future<Output = Result<Webhook>> + Send + 'a>>>,
    #[serde(skip)]
    http: &'a Client,
}

impl<'a> CreateWebhook<'a> {
    pub(crate) fn new(
        http: &'a Client,
        channel_id: impl Into<ChannelId>,
        name: impl Into<String>,
    ) -> Self {
        Self {
            avatar: None,
            channel_id: channel_id.into(),
            fut: None,
            http,
            name: name.into(),
        }
    }

    pub fn avatar(mut self, avatar: impl Into<String>) -> Self {
        self.avatar.replace(avatar.into());

        self
    }

    fn start(&mut self) -> Result<()> {
        self.fut.replace(Box::pin(self.http.request(Request::from((
            serde_json::to_vec(self)?,
            Route::CreateWebhook {
                channel_id: self.channel_id.0,
            },
        )))));

        Ok(())
    }
}

poll_req!(CreateWebhook<'_>, Webhook);
