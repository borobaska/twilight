use super::prelude::*;
use dawn_model::{guild::PartialGuild, id::GuildId};

#[derive(Serialize)]
pub struct GetCurrentUserGuilds<'a> {
    after: Option<GuildId>,
    before: Option<GuildId>,
    #[serde(skip)]
    fut: Option<Pin<Box<dyn Future<Output = Result<Vec<PartialGuild>>> + Send + 'a>>>,
    #[serde(skip)]
    http: &'a Client,
    limit: Option<u64>,
}

impl<'a> GetCurrentUserGuilds<'a> {
    pub(crate) fn new(http: &'a Client) -> Self {
        Self {
            after: None,
            before: None,
            fut: None,
            http,
            limit: None,
        }
    }

    pub fn after(mut self, guild_id: GuildId) -> Self {
        self.after.replace(guild_id);

        self
    }

    pub fn before(mut self, guild_id: GuildId) -> Self {
        self.before.replace(guild_id);

        self
    }

    pub fn limit(mut self, limit: u64) -> Self {
        self.limit.replace(limit);

        self
    }

    fn start(&mut self) -> Result<()> {
        self.fut.replace(Box::pin(self.http.request(Request::from((
            serde_json::to_vec(self)?,
            Route::GetGuilds {
                after: self.after.map(|x| x.0),
                before: self.before.map(|x| x.0),
                limit: self.limit,
            },
        )))));

        Ok(())
    }
}

poll_req!(GetCurrentUserGuilds<'_>, Vec<PartialGuild>);
