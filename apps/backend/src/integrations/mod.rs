use anyhow::{anyhow, bail, Result};
use rust_decimal::{prelude::ToPrimitive, Decimal};
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};
use surf::{http::headers::AUTHORIZATION, Client};
use regex::Regex;

use crate::{
    migrator::{MetadataLot, MetadataSource},
    utils::get_base_http_client,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntegrationMedia {
    pub identifier: String,
    pub lot: MetadataLot,
    #[serde(default)]
    pub source: MetadataSource,
    pub progress: i32,
    pub show_season_number: Option<i32>,
    pub show_episode_number: Option<i32>,
    pub podcast_episode_number: Option<i32>,
}

#[derive(Debug)]
pub struct IntegrationService;

impl IntegrationService {
    pub fn new() -> Self {
        Self
    }

    pub async fn jellyfin_progress(&self, payload: &str) -> Result<IntegrationMedia> {
        mod models {
            use super::*;

            #[derive(Serialize, Deserialize, Debug, Clone)]
            #[serde(rename_all = "PascalCase")]
            pub struct JellyfinWebhookSessionPlayStatePayload {
                pub position_ticks: Decimal,
            }
            #[derive(Serialize, Deserialize, Debug, Clone)]
            #[serde(rename_all = "PascalCase")]
            pub struct JellyfinWebhookSessionPayload {
                pub play_state: JellyfinWebhookSessionPlayStatePayload,
            }
            #[derive(Serialize, Deserialize, Debug, Clone)]
            #[serde(rename_all = "PascalCase")]
            pub struct JellyfinWebhookItemProviderIdsPayload {
                pub tmdb: Option<String>,
            }
            #[derive(Serialize, Deserialize, Debug, Clone)]
            #[serde(rename_all = "PascalCase")]
            pub struct JellyfinWebhookItemPayload {
                pub run_time_ticks: Decimal,
                #[serde(rename = "Type")]
                pub item_type: String,
                pub provider_ids: JellyfinWebhookItemProviderIdsPayload,
                #[serde(rename = "ParentIndexNumber")]
                pub season_number: Option<i32>,
                #[serde(rename = "IndexNumber")]
                pub episode_number: Option<i32>,
            }
            #[derive(Serialize, Deserialize, Debug, Clone)]
            #[serde(rename_all = "PascalCase")]
            pub struct JellyfinWebhookPayload {
                pub event: Option<String>,
                pub item: JellyfinWebhookItemPayload,
                pub series: Option<JellyfinWebhookItemPayload>,
                pub session: JellyfinWebhookSessionPayload,
            }
        }

        let payload = serde_json::from_str::<models::JellyfinWebhookPayload>(payload)?;
        let identifier = if let Some(id) = payload.item.provider_ids.tmdb.as_ref() {
            Some(id.clone())
        } else {
            payload
                .series
                .as_ref()
                .and_then(|s| s.provider_ids.tmdb.clone())
        };
        if identifier.is_none() {
            bail!("No TMDb ID associated with this media")
        }
        let identifier = identifier.unwrap();
        let lot = match payload.item.item_type.as_str() {
            "Episode" => MetadataLot::Show,
            "Movie" => MetadataLot::Movie,
            _ => bail!("Only movies and shows supported"),
        };
        Ok(IntegrationMedia {
            identifier,
            lot,
            source: MetadataSource::Tmdb,
            progress: (payload.session.play_state.position_ticks / payload.item.run_time_ticks
                * dec!(100))
            .to_i32()
            .unwrap(),
            podcast_episode_number: None,
            show_season_number: payload.item.season_number,
            show_episode_number: payload.item.episode_number,
        })
    }

    pub async fn plex_progress(&self, payload: &str, plex_user: &str) -> Result<IntegrationMedia> {
        mod models {
            use super::*;

            #[derive(Serialize, Deserialize, Debug, Clone)]
            pub struct PlexWebhookMetadataGuid {
                #[serde(rename = "id")]
                pub id: String,
            }
            #[derive(Serialize, Deserialize, Debug, Clone)]
            pub struct PlexWebhookMetadataPayload {               
                #[serde(rename = "type")]
                pub item_type: String,
                #[serde(rename = "viewOffset")]
                pub view_offset: Option<Decimal>,
                pub duration: Decimal,                
                #[serde(rename = "parentIndex")]
                pub season_number: Option<i32>,
                #[serde(rename = "index")]
                pub episode_number: Option<i32>,
                #[serde(rename = "Guid")]
                pub guids: Vec<PlexWebhookMetadataGuid>,
            }
            #[derive(Serialize, Deserialize, Debug, Clone)]
            pub struct PlexWebhookAccount {
                #[serde(rename = "title")]
                pub plex_user: String,
            }
            #[derive(Serialize, Deserialize, Debug, Clone)]
            pub struct PlexWebhookPayload {
                #[serde(rename = "event")]
                pub event_type: String,
                pub user: bool,
                pub owner: bool,
                #[serde(rename = "Metadata")]
                pub metadata: PlexWebhookMetadataPayload,
                #[serde(rename = "Account")]
                pub account: PlexWebhookAccount,
            }
        }
                
        let payload_regex = Regex::new(r#"\{.*\}"#).unwrap();
        let json_payload = payload_regex.find(payload)
            .map(|x| x.as_str())
            .unwrap_or("");
        
        let payload = match serde_json::from_str::<models::PlexWebhookPayload>(json_payload) {
            Result::Ok(val) => val,
            Result::Err(err) => bail!("Error during JSON payload deserialisation {}", err),
        };
        if plex_user != payload.account.plex_user {
            bail!("Ignoring non matching user {}", payload.account.plex_user);
        }
        match payload.event_type.as_str() {
            "media.play" | "media.scrobble" | "media.resume" => (),
            _ => bail!("Ignoring event type {}", payload.event_type)
        }

        let tmdb_guid = payload
            .metadata
            .guids
            .into_iter()
            .find(|g| g.id.starts_with("tmdb://"));

        if tmdb_guid.is_none() {
            bail!("No TMDb ID associated with this media")
        }
        let tmdb_guid = tmdb_guid.unwrap();
        let identifier = &tmdb_guid.id[7..];
        let lot = match payload.metadata.item_type.as_str() {
            "movie" => MetadataLot::Movie,
            "episode" => MetadataLot::Show,
            _ => bail!("Only movies and shows supported"),
        };
        let progress = match payload.metadata.view_offset {
            Some(offset) => (offset / payload.metadata.duration * dec!(100)).to_i32().unwrap(),
            None => 0
        };
        
        Ok(IntegrationMedia {
            identifier: identifier.to_owned(),
            lot,
            source: MetadataSource::Tmdb,
            progress,
            podcast_episode_number: None,
            show_season_number: payload.metadata.season_number,
            show_episode_number: payload.metadata.episode_number,
        })
    }

    pub async fn kodi_progress(&self, payload: &str) -> Result<IntegrationMedia> {
        let mut payload = match serde_json::from_str::<IntegrationMedia>(payload) {
            Result::Ok(val) => val,
            Result::Err(err) => bail!(err),
        };
        payload.source = MetadataSource::Tmdb;
        Ok(payload)
    }

    pub async fn audiobookshelf_progress(
        &self,
        base_url: &str,
        access_token: &str,
    ) -> Result<Vec<IntegrationMedia>> {
        mod models {
            use super::*;

            #[derive(Debug, Serialize, Deserialize)]
            pub struct ItemProgress {
                pub progress: Decimal,
            }
            #[derive(Debug, Serialize, Deserialize)]
            pub struct ItemMetadata {
                pub asin: Option<String>,
            }
            #[derive(Debug, Serialize, Deserialize)]
            pub struct ItemMedia {
                pub metadata: ItemMetadata,
            }
            #[derive(Debug, Serialize, Deserialize)]
            pub struct Item {
                pub id: String,
                pub media: ItemMedia,
            }
            #[derive(Debug, Serialize, Deserialize)]
            #[serde(rename_all = "camelCase")]
            pub struct Response {
                pub library_items: Vec<Item>,
            }
        }
        let client: Client = get_base_http_client(
            &format!("{}/api/", base_url),
            vec![(AUTHORIZATION, format!("Bearer {access_token}"))],
        );
        let resp: models::Response = client
            .get("me/items-in-progress")
            .await
            .map_err(|e| anyhow!(e))?
            .body_json()
            .await
            .unwrap();
        let mut media_items = vec![];
        for item in resp.library_items.iter() {
            if let Some(asin) = item.media.metadata.asin.clone() {
                let resp: models::ItemProgress = client
                    .get(format!("me/progress/{}", item.id))
                    .await
                    .map_err(|e| anyhow!(e))?
                    .body_json()
                    .await
                    .unwrap();
                media_items.push(IntegrationMedia {
                    identifier: asin,
                    lot: MetadataLot::AudioBook,
                    source: MetadataSource::Audible,
                    progress: (resp.progress * dec!(100)).to_i32().unwrap(),
                    show_season_number: None,
                    show_episode_number: None,
                    podcast_episode_number: None,
                });
            }
        }
        Ok(media_items)
    }
}
