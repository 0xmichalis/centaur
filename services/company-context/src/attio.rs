use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, NaiveDate, NaiveDateTime, SecondsFormat, TimeZone, Utc};
use chrono_tz::Tz;
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Deserializer, Serialize, de::DeserializeOwned};

use crate::{
    config::Config,
    errors::{denied, rejected},
    telemetry,
};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// The largest page Attio's list endpoints return.
const PAGE_SIZE: &str = "200";

/// Talks to Attio's REST API with a workspace credential.
#[derive(Clone)]
pub struct AttioClient {
    http: Client,
    base_url: String,
}

/// Reads `null` as the default, like a missing field. The meetings API is in
/// beta, so one meeting with an unexpected `null` must not fail its page.
fn nullable<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

/// A meeting as Attio lists it. Fields are defaulted because the meetings API
/// is in beta.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(default)]
pub struct Meeting {
    #[serde(deserialize_with = "nullable")]
    pub id: MeetingId,
    #[serde(deserialize_with = "nullable")]
    pub title: String,
    #[serde(deserialize_with = "nullable")]
    pub description: String,
    #[serde(deserialize_with = "nullable")]
    pub start: MeetingTime,
    #[serde(deserialize_with = "nullable")]
    pub end: MeetingTime,
    #[serde(deserialize_with = "nullable")]
    pub participants: Vec<Participant>,
    #[serde(deserialize_with = "nullable")]
    pub linked_records: Vec<LinkedRecord>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(default)]
pub struct MeetingId {
    #[serde(deserialize_with = "nullable")]
    pub meeting_id: String,
}

/// A `datetime` for a timed meeting, or a `date` for an all-day one, with the
/// IANA timezone the meeting was scheduled in, if known.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(default)]
pub struct MeetingTime {
    pub datetime: Option<String>,
    pub date: Option<String>,
    pub timezone: Option<String>,
}

impl MeetingTime {
    /// The instant. A datetime without an offset is local to the timezone, or
    /// UTC without one; all-day dates are taken as UTC midnight, as Attio
    /// filters them by default.
    pub fn instant(&self) -> Option<DateTime<Utc>> {
        if let Some(datetime) = self.datetime.as_deref() {
            if let Ok(datetime) = DateTime::parse_from_rfc3339(datetime) {
                return Some(datetime.with_timezone(&Utc));
            }
            let local = NaiveDateTime::parse_from_str(datetime, "%Y-%m-%dT%H:%M:%S%.f").ok()?;
            return match self
                .timezone
                .as_deref()
                .and_then(|zone| zone.parse::<Tz>().ok())
            {
                // The earlier instant when a clock change repeats the time.
                Some(zone) => zone
                    .from_local_datetime(&local)
                    .earliest()
                    .map(|datetime| datetime.with_timezone(&Utc)),
                None => Some(local.and_utc()),
            };
        }
        let date = NaiveDate::parse_from_str(self.date.as_deref()?, "%Y-%m-%d").ok()?;
        Some(date.and_hms_opt(0, 0, 0)?.and_utc())
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(default)]
pub struct Participant {
    pub email_address: Option<String>,
    pub name: Option<String>,
    #[serde(deserialize_with = "nullable")]
    pub is_organizer: bool,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(default)]
pub struct LinkedRecord {
    #[serde(deserialize_with = "nullable")]
    pub object_slug: String,
    #[serde(deserialize_with = "nullable")]
    pub record_id: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct CallRecording {
    #[serde(deserialize_with = "nullable")]
    pub id: CallRecordingId,
    #[serde(deserialize_with = "nullable")]
    pub status: String,
    #[serde(deserialize_with = "nullable")]
    pub web_url: String,
    pub transcript: Option<Transcript>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct CallRecordingId {
    #[serde(deserialize_with = "nullable")]
    pub call_recording_id: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Transcript {
    #[serde(deserialize_with = "nullable")]
    pub segments: Vec<Segment>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Segment {
    #[serde(deserialize_with = "nullable")]
    pub speech: String,
    #[serde(deserialize_with = "nullable")]
    pub speaker: Speaker,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Speaker {
    #[serde(deserialize_with = "nullable")]
    pub name: String,
}

#[derive(Deserialize)]
struct Page<T> {
    data: Vec<T>,
    #[serde(default)]
    pagination: Pagination,
}

#[derive(Default, Deserialize)]
struct Pagination {
    next_cursor: Option<String>,
}

#[derive(Deserialize)]
struct Single<T> {
    data: T,
}

impl AttioClient {
    pub fn new(config: &Config) -> Result<Self> {
        Ok(Self {
            http: Client::builder().timeout(REQUEST_TIMEOUT).build()?,
            base_url: config.attio_api_base_url.clone(),
        })
    }

    /// Lists the meetings that end at or after `ends_from` and start before
    /// `starts_before`.
    pub async fn list_meetings(
        &self,
        token: &str,
        ends_from: DateTime<Utc>,
        starts_before: DateTime<Utc>,
    ) -> Result<Vec<Meeting>> {
        let ends_from = ends_from.to_rfc3339_opts(SecondsFormat::Secs, true);
        let starts_before = starts_before.to_rfc3339_opts(SecondsFormat::Secs, true);
        self.list(
            token,
            "list_meetings",
            "/meetings",
            &[
                ("sort", "start_asc"),
                ("ends_from", &ends_from),
                ("starts_before", &starts_before),
            ],
        )
        .await
    }

    pub async fn list_call_recordings(
        &self,
        token: &str,
        meeting_id: &str,
    ) -> Result<Vec<CallRecording>> {
        self.list(
            token,
            "list_call_recordings",
            &format!("/meetings/{meeting_id}/call_recordings"),
            &[],
        )
        .await
    }

    /// Gets a call recording with its transcript.
    pub async fn call_recording(
        &self,
        token: &str,
        meeting_id: &str,
        call_recording_id: &str,
    ) -> Result<CallRecording> {
        let single: Single<CallRecording> = self
            .get(
                token,
                "get_call_recording",
                &format!("/meetings/{meeting_id}/call_recordings/{call_recording_id}"),
                &[],
            )
            .await?;
        Ok(single.data)
    }

    async fn list<T: DeserializeOwned>(
        &self,
        token: &str,
        operation: &str,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<Vec<T>> {
        let mut items = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut params = query.to_vec();
            params.push(("limit", PAGE_SIZE));
            if let Some(cursor) = &cursor {
                params.push(("cursor", cursor));
            }
            let page: Page<T> = self.get(token, operation, path, &params).await?;
            items.extend(page.data);
            match page.pagination.next_cursor {
                Some(next) if !next.is_empty() && cursor.as_ref() != Some(&next) => {
                    cursor = Some(next);
                }
                _ => return Ok(items),
            }
        }
    }

    async fn get<T: DeserializeOwned>(
        &self,
        token: &str,
        operation: &str,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<T> {
        let started = Instant::now();
        let response = self
            .http
            .get(format!("{}{path}", self.base_url))
            .bearer_auth(token)
            .query(query)
            .send()
            .await;
        telemetry::upstream_response("attio", operation, started, &response);
        let response = response.context("send Attio request")?;
        let status = response.status();
        if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
            bail!("Attio {operation} returned HTTP {status}");
        }
        // The credential, not the item, was refused: revoked, or missing a scope.
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            return Err(denied(format!("Attio {operation} returned HTTP {status}")));
        }
        if !status.is_success() {
            return Err(rejected(format!(
                "Attio {operation} returned HTTP {status}"
            )));
        }
        let body = response.bytes().await.context("read Attio response")?;
        serde_json::from_slice(&body).map_err(|error| {
            rejected(format!(
                "Attio {operation} returned malformed JSON: {error}"
            ))
        })
    }
}

/// A fake Attio API. `token-1` reads everything; `no-recordings` can list
/// meetings but not call recordings, as without `call_recording:read`; any
/// other token is unauthorized.
#[cfg(test)]
pub(crate) mod fake {
    use std::collections::HashMap;

    use axum::{
        Json, Router,
        extract::{Path, Query},
        http::{HeaderMap, StatusCode},
        response::{IntoResponse, Response},
        routing::get,
    };
    use serde_json::{Value, json};

    use super::*;

    fn token(headers: &HeaderMap) -> &str {
        headers["authorization"]
            .to_str()
            .unwrap()
            .trim_start_matches("Bearer ")
    }

    async fn meetings(
        headers: HeaderMap,
        Query(query): Query<HashMap<String, String>>,
    ) -> Response {
        if !["token-1", "no-recordings"].contains(&token(&headers)) {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        assert_eq!(query["sort"], "start_asc");
        assert!(query.contains_key("ends_from") && query.contains_key("starts_before"));
        let page = match query.get("cursor").map(String::as_str) {
            None => json!({
                "data": [{
                    "id": { "workspace_id": "ws-1", "meeting_id": "m-1" },
                    "title": "Planning",
                    "description": "",
                    "is_all_day": false,
                    "start": { "datetime": "2026-07-02T09:00:00+02:00", "timezone": "Europe/Berlin" },
                    "end": { "datetime": "2026-07-02T10:00:00", "timezone": "Europe/Berlin" },
                    "participants": [
                        { "status": "accepted", "is_organizer": true, "email_address": "ada@example.com", "name": null }
                    ],
                    "linked_records": [],
                    "created_at": "2026-07-01T00:00:00Z",
                    "created_by_actor": { "type": "system", "id": null }
                }],
                "pagination": { "next_cursor": "c-2" }
            }),
            // Unexpected nulls, as a beta API may return.
            Some("c-2") => json!({
                "data": [{
                    "id": { "workspace_id": "ws-1", "meeting_id": "m-2" },
                    "title": null,
                    "description": null,
                    "start": { "date": "2026-07-03" },
                    "end": { "date": "2026-07-04" },
                    "participants": null,
                    "linked_records": null
                }],
                "pagination": { "next_cursor": null }
            }),
            Some(_) => return StatusCode::BAD_REQUEST.into_response(),
        };
        Json(page).into_response()
    }

    async fn recordings(headers: HeaderMap, Path(meeting_id): Path<String>) -> Response {
        if token(&headers) != "token-1" {
            return StatusCode::FORBIDDEN.into_response();
        }
        let recording = |id: &str, status: &str| {
            json!({
                "id": { "workspace_id": "ws-1", "meeting_id": meeting_id, "call_recording_id": id },
                "status": status,
                "web_url": format!("https://app.attio.com/{id}"),
                "created_by_actor": { "type": "system", "id": null },
                "created_at": "2026-07-02T08:00:00Z"
            })
        };
        let data = match meeting_id.as_str() {
            "m-1" => vec![
                recording("r-1", "completed"),
                recording("r-2", "processing"),
                recording("r-3", "failed"),
            ],
            "m-2" => Vec::new(),
            _ => return StatusCode::NOT_FOUND.into_response(),
        };
        Json(json!({ "data": data, "pagination": { "next_cursor": null } })).into_response()
    }

    async fn recording(
        headers: HeaderMap,
        Path((meeting_id, recording_id)): Path<(String, String)>,
    ) -> Response {
        if token(&headers) != "token-1" {
            return StatusCode::FORBIDDEN.into_response();
        }
        match (meeting_id.as_str(), recording_id.as_str()) {
            ("m-1", "r-1") => Json(json!({ "data": {
                "id": { "workspace_id": "ws-1", "meeting_id": "m-1", "call_recording_id": "r-1" },
                "status": "completed",
                "web_url": "https://app.attio.com/r-1",
                "video_url": null,
                "transcript": { "segments": [
                    { "speech": "Hello.", "start_time": 0.0, "end_time": 1.0, "speaker": { "name": "Ada" } },
                    { "speech": null, "start_time": 1.0, "end_time": 2.0, "speaker": { "name": null } }
                ], "raw_transcript": "Hello." }
            } }))
            .into_response(),
            ("m-busy", _) => StatusCode::TOO_MANY_REQUESTS.into_response(),
            _ => (StatusCode::NOT_FOUND, Json(Value::Null)).into_response(),
        }
    }

    /// Serves the fake until the returned handle is aborted.
    pub(crate) async fn serve() -> (AttioClient, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = Router::new()
            .route("/meetings", get(meetings))
            .route("/meetings/{meeting_id}/call_recordings", get(recordings))
            .route(
                "/meetings/{meeting_id}/call_recordings/{recording_id}",
                get(recording),
            );
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let client = AttioClient {
            http: Client::new(),
            base_url: format!("http://{address}"),
        };
        (client, server)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::errors::{is_denied, is_rejected};

    fn at(value: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(value).unwrap().to_utc()
    }

    #[tokio::test]
    async fn lists_every_page_tolerating_nulls() {
        let (client, server) = fake::serve().await;
        let meetings = client
            .list_meetings(
                "token-1",
                at("2026-07-01T00:00:00Z"),
                at("2026-07-08T00:00:00Z"),
            )
            .await
            .unwrap();
        assert_eq!(
            meetings
                .iter()
                .map(|meeting| meeting.id.meeting_id.as_str())
                .collect::<Vec<_>>(),
            ["m-1", "m-2"]
        );
        assert_eq!(
            meetings[0].start.instant(),
            Some(at("2026-07-02T07:00:00Z"))
        );
        // An end without an offset is local to the meeting's timezone.
        assert_eq!(meetings[0].end.instant(), Some(at("2026-07-02T08:00:00Z")));
        assert_eq!(
            meetings[0].participants[0].email_address.as_deref(),
            Some("ada@example.com")
        );
        assert_eq!(meetings[1].title, "");
        assert!(meetings[1].participants.is_empty());
        assert_eq!(
            meetings[1].start.instant(),
            Some(at("2026-07-03T00:00:00Z"))
        );

        let recording = client
            .call_recording("token-1", "m-1", "r-1")
            .await
            .unwrap();
        let segments = recording.transcript.unwrap().segments;
        assert_eq!(segments[0].speaker.name, "Ada");
        assert_eq!(segments[1].speech, "");
        server.abort();
    }

    #[tokio::test]
    async fn classifies_failures_by_what_was_refused() {
        let (client, server) = fake::serve().await;
        let missing = client
            .call_recording("token-1", "m-gone", "r-1")
            .await
            .unwrap_err();
        assert!(is_rejected(&missing), "a missing item is not retried");
        let limited = client
            .call_recording("token-1", "m-busy", "r-1")
            .await
            .unwrap_err();
        assert!(
            !is_rejected(&limited) && !is_denied(&limited),
            "rate limits are retried"
        );
        for refused in [
            client
                .list_call_recordings("no-recordings", "m-1")
                .await
                .unwrap_err(),
            client
                .list_meetings(
                    "revoked",
                    at("2026-07-01T00:00:00Z"),
                    at("2026-07-08T00:00:00Z"),
                )
                .await
                .unwrap_err(),
        ] {
            assert!(is_denied(&refused) && !is_rejected(&refused), "{refused}");
        }
        server.abort();
    }

    #[test]
    fn datetimes_without_an_offset_or_known_timezone_are_utc() {
        let time = |datetime: &str, timezone: Option<&str>| MeetingTime {
            datetime: Some(datetime.to_owned()),
            date: None,
            timezone: timezone.map(str::to_owned),
        };
        assert_eq!(
            time("2026-07-02T09:00:00", None).instant(),
            Some(at("2026-07-02T09:00:00Z"))
        );
        assert_eq!(
            time("2026-07-02T09:00:00", Some("Not/AZone")).instant(),
            Some(at("2026-07-02T09:00:00Z"))
        );
        assert_eq!(
            time("2026-01-02T09:00:00", Some("America/New_York")).instant(),
            Some(at("2026-01-02T14:00:00Z"))
        );
        assert_eq!(MeetingTime::default().instant(), None);
    }
}
