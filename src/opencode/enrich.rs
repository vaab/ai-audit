use anyhow::Result;

use crate::provider::Provider;
use crate::session_filter::{EnrichSessionsLive, EnrichSessionsStatic, EnrichedSession};

use super::server_client::{compute_live, LiveStatus, ServerClient, ServerCredentials};
use super::status::{classify_static, fetch_last_message_meta, LastMessageMeta, StaticStatus};

#[derive(Debug, Clone)]
pub struct StaticExtension {
    pub status: StaticStatus,
    pub meta: LastMessageMeta,
}

#[derive(Debug, Clone)]
pub struct LiveExtension {
    pub status: LiveStatus,
}

struct StaticEnricher;

impl EnrichSessionsStatic for StaticEnricher {
    fn enrich(&self, sessions: &mut [EnrichedSession]) -> Result<()> {
        let session_ids = sessions
            .iter()
            .filter(|session| session.base.provider == Provider::OpenCode)
            .map(|session| session.base.session_id.clone())
            .collect::<Vec<_>>();

        if session_ids.is_empty() {
            return Ok(());
        }

        let conn = super::db::open_db()?;
        let meta = fetch_last_message_meta(&conn, &session_ids)?;
        for session in sessions.iter_mut() {
            if session.base.provider != Provider::OpenCode {
                continue;
            }
            let Some(found) = meta.get(&session.base.session_id).cloned() else {
                continue;
            };
            session.last_message_ts = Some(found.last_msg_ts);
            session.static_ext = Some(Box::new(StaticExtension {
                status: classify_static(&found),
                meta: found,
            }));
        }
        Ok(())
    }
}

struct LiveEnricher {
    client: ServerClient,
}

impl EnrichSessionsLive for LiveEnricher {
    fn enrich(&self, sessions: &mut [EnrichedSession]) -> Result<()> {
        let map = self.client.session_status()?;
        let server_unreachable = map.is_none();
        for session in sessions.iter_mut() {
            if session.base.provider != Provider::OpenCode {
                continue;
            }
            session.live_ext = Some(Box::new(LiveExtension {
                status: compute_live(&session.base.session_id, map.as_ref(), server_unreachable),
            }));
        }
        Ok(())
    }
}

pub fn make_static_enricher() -> Box<dyn EnrichSessionsStatic> {
    Box::new(StaticEnricher)
}

pub fn make_live_enricher(creds: ServerCredentials) -> Box<dyn EnrichSessionsLive> {
    Box::new(LiveEnricher {
        client: ServerClient::new(creds),
    })
}

pub fn static_status_predicate(
    statuses: Vec<StaticStatus>,
) -> Box<dyn Fn(&EnrichedSession) -> bool> {
    Box::new(move |session| {
        extract_static(session)
            .map(|extension| statuses.contains(&extension.status))
            .unwrap_or(false)
    })
}

pub fn live_status_predicate(statuses: Vec<LiveStatus>) -> Box<dyn Fn(&EnrichedSession) -> bool> {
    Box::new(move |session| {
        extract_live(session)
            .map(|extension| statuses.contains(&extension.status))
            .unwrap_or(false)
    })
}

pub fn extract_static(session: &EnrichedSession) -> Option<&StaticExtension> {
    session
        .static_ext
        .as_ref()?
        .downcast_ref::<StaticExtension>()
}

pub fn extract_live(session: &EnrichedSession) -> Option<&LiveExtension> {
    session.live_ext.as_ref()?.downcast_ref::<LiveExtension>()
}
