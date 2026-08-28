// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct GiphyResponse {
    pub data: Vec<GiphyGif>,
    pub pagination: GiphyPagination,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GiphyGif {
    pub id: String,
    pub title: String,
    pub images: GiphyImages,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GiphyImages {
    pub fixed_height: GiphyImage,
    pub fixed_height_still: GiphyImage,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GiphyImage {
    pub url: String,
    pub width: String,
    pub height: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GiphyPagination {
    pub total_count: u32,
    pub count: u32,
    pub offset: u32,
}

pub struct GiphyService {
    client: reqwest::Client,
    api_key: String,
}

impl GiphyService {
    pub fn new(api_key: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key,
        }
    }

    pub async fn search(
        &self,
        query: &str,
        limit: u32,
        offset: u32,
    ) -> anyhow::Result<GiphyResponse> {
        let resp = self
            .client
            .get("https://api.giphy.com/v1/gifs/search")
            .query(&[
                ("api_key", self.api_key.as_str()),
                ("q", query),
                ("rating", "g"),
            ])
            .query(&[("limit", limit), ("offset", offset)])
            .send()
            .await?
            .error_for_status()?
            .json::<GiphyResponse>()
            .await?;
        Ok(resp)
    }

    pub async fn trending(&self, limit: u32, offset: u32) -> anyhow::Result<GiphyResponse> {
        let resp = self
            .client
            .get("https://api.giphy.com/v1/gifs/trending")
            .query(&[("api_key", self.api_key.as_str()), ("rating", "g")])
            .query(&[("limit", limit), ("offset", offset)])
            .send()
            .await?
            .error_for_status()?
            .json::<GiphyResponse>()
            .await?;
        Ok(resp)
    }
}
