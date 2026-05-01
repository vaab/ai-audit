use anyhow::{anyhow, Result};
use std::any::Any;
use std::path::PathBuf;

use crate::cli::def::SessionType;
use crate::provider::{self, Provider, Session, SessionProvider};

/// Predicate over an enriched session. Boxed to allow heterogeneous closures
/// at filter-construction sites without leaking generics into callers.
pub type SessionPredicate = Box<dyn Fn(&EnrichedSession) -> bool>;

pub struct SessionFilter {
    pub session_type: Option<SessionType>,
    pub session_id: Option<String>,
    pub project: Option<String>,
    pub search: Option<String>,
    pub file: Option<String>,
    pub timespan: Option<(i64, i64)>,
    pub last_message_in: Option<(i64, i64)>,
    pub all: bool,
    pub children_of: Option<String>,
    pub static_enrich: Option<Box<dyn EnrichSessionsStatic>>,
    pub static_predicate: Option<SessionPredicate>,
    pub live_enrich: Option<Box<dyn EnrichSessionsLive>>,
    pub live_predicate: Option<SessionPredicate>,
}

pub trait EnrichSessionsStatic: Send + Sync {
    fn enrich(&self, sessions: &mut [EnrichedSession]) -> Result<()>;
}

pub trait EnrichSessionsLive: Send + Sync {
    fn enrich(&self, sessions: &mut [EnrichedSession]) -> Result<()>;
}

pub struct EnrichedSession {
    pub base: Session,
    pub last_message_ts: Option<i64>,
    pub static_ext: Option<Box<dyn Any + Send + Sync>>,
    pub live_ext: Option<Box<dyn Any + Send + Sync>>,
}

pub fn list_filtered(filter: &SessionFilter) -> Result<Vec<EnrichedSession>> {
    let providers = match filter.session_type {
        Some(SessionType::ClaudeCode) => vec![provider::provider_for(Provider::ClaudeCode)],
        Some(SessionType::OpenCode) => vec![provider::provider_for(Provider::OpenCode)],
        None => provider::all_providers(),
    };
    list_filtered_from_providers(filter, providers)
}

fn list_filtered_from_providers(
    filter: &SessionFilter,
    providers: Vec<Box<dyn SessionProvider>>,
) -> Result<Vec<EnrichedSession>> {
    let mut sessions = providers
        .iter()
        .flat_map(|provider| provider.list_sessions().unwrap_or_default())
        .filter(|session| matches_primary_filters(session, filter))
        .map(|base| EnrichedSession {
            base,
            last_message_ts: None,
            static_ext: None,
            live_ext: None,
        })
        .collect::<Vec<_>>();

    sessions.retain(|session| matches_medium_filters(session, filter, &providers));

    if filter.static_enrich.is_some() || filter.last_message_in.is_some() {
        let enricher = filter
            .static_enrich
            .as_ref()
            .ok_or_else(|| anyhow!("last_message_in requires static enrichment"))?;
        enricher.enrich(&mut sessions)?;
    }

    if let Some((start, end)) = filter.last_message_in {
        sessions.retain(|session| {
            session
                .last_message_ts
                .is_some_and(|timestamp| timestamp >= start && timestamp < end)
        });
    }

    if let Some(predicate) = filter.static_predicate.as_ref() {
        sessions.retain(|session| predicate(session));
    }

    if let Some(enricher) = filter.live_enrich.as_ref() {
        enricher.enrich(&mut sessions)?;
    }

    if let Some(predicate) = filter.live_predicate.as_ref() {
        sessions.retain(|session| predicate(session));
    }

    sessions.sort_by_key(|session| session.base.started_at);
    Ok(sessions)
}

fn matches_primary_filters(session: &Session, filter: &SessionFilter) -> bool {
    if let Some(parent) = filter.children_of.as_deref() {
        return session.parent_id.as_deref() == Some(parent)
            && session_id_matches(session, filter)
            && project_matches(session, filter)
            && timespan_matches(session, filter);
    }

    if filter.session_id.is_some() {
        return session_id_matches(session, filter)
            && project_matches(session, filter)
            && timespan_matches(session, filter);
    }

    if !filter.all && session.parent_id.is_some() {
        return false;
    }

    session_id_matches(session, filter)
        && project_matches(session, filter)
        && timespan_matches(session, filter)
}

fn session_id_matches(session: &Session, filter: &SessionFilter) -> bool {
    filter
        .session_id
        .as_deref()
        .map(|expected| session.session_id == expected)
        .unwrap_or(true)
}

fn project_matches(session: &Session, filter: &SessionFilter) -> bool {
    filter
        .project
        .as_deref()
        .map(|expected| session.project_dir == expected)
        .unwrap_or(true)
}

fn timespan_matches(session: &Session, filter: &SessionFilter) -> bool {
    filter
        .timespan
        .map(|(start, end)| {
            let started = session.base_started();
            let updated = session.base_updated();
            started <= end && updated >= start
        })
        .unwrap_or(true)
}

fn matches_medium_filters(
    session: &EnrichedSession,
    filter: &SessionFilter,
    providers: &[Box<dyn SessionProvider>],
) -> bool {
    let Some(provider) = providers
        .iter()
        .find(|provider| provider.provider() == session.base.provider)
    else {
        return false;
    };
    if let Some(target) = filter.file.as_deref() {
        if !provider.session_edited_file(&session.base.session_id, target) {
            return false;
        }
    }
    if let Some(needle) = filter.search.as_deref() {
        if !provider.session_contains_text(&session.base.session_id, needle) {
            return false;
        }
    }
    true
}

pub fn canonicalize_filter_path(path: &str) -> String {
    let path = PathBuf::from(path);
    let absolute = if path.is_absolute() {
        path
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    absolute
        .canonicalize()
        .unwrap_or(absolute)
        .to_string_lossy()
        .to_string()
}

trait SessionTimes {
    fn base_started(&self) -> i64;
    fn base_updated(&self) -> i64;
}

impl SessionTimes for Session {
    fn base_started(&self) -> i64 {
        self.started_at.timestamp()
    }

    fn base_updated(&self) -> i64 {
        self.updated_at.timestamp()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use chrono::{TimeZone, Utc};
    use std::sync::{Arc, Mutex};

    use crate::provider::{Message, TokenUsage};
    use crate::transcript::TranscriptEntry;

    struct FakeProvider {
        provider: Provider,
        sessions: Vec<Session>,
        file_hits: Vec<String>,
        search_hits: Vec<String>,
    }

    impl SessionProvider for FakeProvider {
        fn provider(&self) -> Provider {
            self.provider
        }

        fn list_sessions(&self) -> Result<Vec<Session>> {
            Ok(self.sessions.clone())
        }

        fn session_contains_text(&self, session_id: &str, _needle: &str) -> bool {
            self.search_hits.iter().any(|hit| hit == session_id)
        }

        fn session_edited_file(&self, session_id: &str, _file_path: &str) -> bool {
            self.file_hits.iter().any(|hit| hit == session_id)
        }

        fn session_tail_contains_text(
            &self,
            _session_id: &str,
            _needle: &str,
            _last_n: usize,
        ) -> bool {
            false
        }

        fn parse_transcript(&self, _session_id: &str) -> Result<Vec<TranscriptEntry>> {
            Ok(Vec::new())
        }

        fn list_messages(&self, _session_id: &str) -> Result<Vec<Message>> {
            Ok(vec![Message {
                message_id: "msg".to_string(),
                session_id: "ses".to_string(),
                provider: self.provider,
                role: "assistant".to_string(),
                model: None,
                timestamp: Utc::now(),
                tokens: Some(TokenUsage::default()),
            }])
        }
    }

    struct SpyStatic {
        seen: Arc<Mutex<Vec<usize>>>,
    }

    impl EnrichSessionsStatic for SpyStatic {
        fn enrich(&self, sessions: &mut [EnrichedSession]) -> Result<()> {
            self.seen.lock().unwrap().push(sessions.len());
            for (index, session) in sessions.iter_mut().enumerate() {
                session.last_message_ts = Some(100 + index as i64);
                session.static_ext = Some(Box::new(index));
            }
            Ok(())
        }
    }

    struct SpyLive {
        seen: Arc<Mutex<Vec<usize>>>,
        fail_if_called: bool,
    }

    impl EnrichSessionsLive for SpyLive {
        fn enrich(&self, sessions: &mut [EnrichedSession]) -> Result<()> {
            if self.fail_if_called {
                panic!("live enrich called unexpectedly");
            }
            self.seen.lock().unwrap().push(sessions.len());
            for session in sessions.iter_mut() {
                session.live_ext = Some(Box::new(true));
            }
            Ok(())
        }
    }

    fn session(id: &str, parent_id: Option<&str>) -> Session {
        Session {
            session_id: id.to_string(),
            provider: Provider::OpenCode,
            started_at: Utc.timestamp_opt(10, 0).unwrap(),
            updated_at: Utc.timestamp_opt(20, 0).unwrap(),
            project_dir: "/tmp/project".to_string(),
            title: id.to_string(),
            parent_id: parent_id.map(str::to_string),
        }
    }

    #[test]
    fn combined_filters_intersect() {
        let filter = SessionFilter {
            session_type: Some(SessionType::OpenCode),
            session_id: None,
            project: Some("/tmp/project".to_string()),
            search: Some("needle".to_string()),
            file: Some("/tmp/project/file.rs".to_string()),
            timespan: Some((0, 30)),
            last_message_in: None,
            all: false,
            children_of: None,
            static_enrich: None,
            static_predicate: None,
            live_enrich: None,
            live_predicate: None,
        };

        let sessions = list_filtered_from_providers(
            &filter,
            vec![Box::new(FakeProvider {
                provider: Provider::OpenCode,
                sessions: vec![session("ses_hit", None), session("ses_miss", None)],
                file_hits: vec!["ses_hit".to_string()],
                search_hits: vec!["ses_hit".to_string()],
            })],
        )
        .unwrap();

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].base.session_id, "ses_hit");
    }

    #[test]
    fn enrichers_only_see_survivors_in_order() {
        let static_seen = Arc::new(Mutex::new(Vec::new()));
        let live_seen = Arc::new(Mutex::new(Vec::new()));
        let filter = SessionFilter {
            session_type: Some(SessionType::OpenCode),
            session_id: None,
            project: None,
            search: None,
            file: Some("/tmp/project/file.rs".to_string()),
            timespan: None,
            last_message_in: None,
            all: false,
            children_of: None,
            static_enrich: Some(Box::new(SpyStatic {
                seen: Arc::clone(&static_seen),
            })),
            static_predicate: Some(Box::new(|session| session.base.session_id == "ses_b")),
            live_enrich: Some(Box::new(SpyLive {
                seen: Arc::clone(&live_seen),
                fail_if_called: false,
            })),
            live_predicate: None,
        };

        let sessions = list_filtered_from_providers(
            &filter,
            vec![Box::new(FakeProvider {
                provider: Provider::OpenCode,
                sessions: vec![
                    session("ses_a", None),
                    session("ses_b", None),
                    session("ses_c", None),
                ],
                file_hits: vec!["ses_a".to_string(), "ses_b".to_string()],
                search_hits: Vec::new(),
            })],
        )
        .unwrap();

        assert_eq!(*static_seen.lock().unwrap(), vec![2]);
        assert_eq!(*live_seen.lock().unwrap(), vec![1]);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].base.session_id, "ses_b");
    }

    #[test]
    fn live_enrichment_is_not_called_when_absent() {
        let filter = SessionFilter {
            session_type: Some(SessionType::OpenCode),
            session_id: None,
            project: None,
            search: None,
            file: None,
            timespan: None,
            last_message_in: None,
            all: false,
            children_of: None,
            static_enrich: None,
            static_predicate: None,
            live_enrich: None,
            live_predicate: None,
        };

        let sessions = list_filtered_from_providers(
            &filter,
            vec![Box::new(FakeProvider {
                provider: Provider::OpenCode,
                sessions: vec![session("ses_a", None)],
                file_hits: Vec::new(),
                search_hits: Vec::new(),
            })],
        )
        .unwrap();

        assert_eq!(sessions.len(), 1);
    }
}
