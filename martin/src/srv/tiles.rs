use actix_http::header::{Quality, QualityItem};
use actix_http::ContentEncoding;
use actix_web::error::{ErrorBadRequest, ErrorNotFound};
use actix_web::http::header::{
    AcceptEncoding, Encoding as ActixEncoding, Encoding as HeaderEnc, Preference, CONTENT_ENCODING,
};
use actix_web::web::{Data, Path, Query};
use actix_web::{route, HttpMessage, HttpRequest, HttpResponse, Result as ActixResult};
use futures::future::try_join_all;
use log::trace;
use martin_tile_utils::{Encoding, Format, TileInfo};
use serde::Deserialize;

use crate::args::PreferredEncoding;
use crate::source::{Source, TileSources, UrlQuery};
use crate::srv::server::map_internal_error;
use crate::srv::SrvConfig;
use crate::utils::cache::get_or_insert_cached_value;
use crate::utils::{
    decode_brotli, decode_gzip, encode_brotli, encode_gzip, CacheKey, CacheValue, MainCache,
    OptMainCache,
};
use crate::{Tile, TileCoord, TileData};

static SUPPORTED_ENCODINGS: &[HeaderEnc] = &[
    HeaderEnc::gzip(),
    HeaderEnc::brotli(),
    HeaderEnc::identity(),
];

#[derive(Deserialize, Clone)]
pub struct TileRequest {
    source_ids: String,
    z: u8,
    x: u32,
    y: u32,
}

#[route("/{source_ids}/{z}/{x}/{y}", method = "GET", method = "HEAD")]
async fn get_tile(
    req: HttpRequest,
    srv_config: Data<SrvConfig>,
    path: Path<TileRequest>,
    sources: Data<TileSources>,
    cache: Data<OptMainCache>,
) -> ActixResult<HttpResponse> {
    let src = DynTileSource::new(
        sources.as_ref(),
        &path.source_ids,
        Some(path.z),
        req.query_string(),
        req.get_header::<AcceptEncoding>(),
        srv_config.preferred_encoding,
        cache.as_ref().as_ref(),
    )?;

    src.get_http_response(TileCoord {
        z: path.z,
        x: path.x,
        y: path.y,
    })
    .await
}

pub struct DynTileSource<'a> {
    pub sources: Vec<&'a dyn Source>,
    pub info: TileInfo,
    pub query_str: Option<&'a str>,
    pub query_obj: Option<UrlQuery>,
    pub accept_enc: Option<AcceptEncoding>,
    pub preferred_enc: Option<PreferredEncoding>,
    pub cache: Option<&'a MainCache>,
}

impl<'a> DynTileSource<'a> {
    pub fn new(
        sources: &'a TileSources,
        source_ids: &str,
        zoom: Option<u8>,
        query: &'a str,
        accept_enc: Option<AcceptEncoding>,
        preferred_enc: Option<PreferredEncoding>,
        cache: Option<&'a MainCache>,
    ) -> ActixResult<Self> {
        let (sources, use_url_query, info) = sources.get_sources(source_ids, zoom)?;

        if sources.is_empty() {
            return Err(ErrorNotFound("No valid sources found"));
        }

        let mut query_obj = None;
        let mut query_str = None;
        if use_url_query && !query.is_empty() {
            query_obj = Some(Query::<UrlQuery>::from_query(query)?.into_inner());
            query_str = Some(query);
        }

        Ok(Self {
            sources,
            info,
            query_str,
            query_obj,
            accept_enc,
            preferred_enc,
            cache,
        })
    }

    pub async fn get_http_response(&self, xyz: TileCoord) -> ActixResult<HttpResponse> {
        let tile = self.get_tile_content(xyz).await?;

        Ok(if tile.data.is_empty() {
            HttpResponse::NoContent().finish()
        } else {
            let mut response = HttpResponse::Ok();
            response.content_type(tile.info.format.content_type());
            if let Some(val) = tile.info.encoding.content_encoding() {
                response.insert_header((CONTENT_ENCODING, val));
            }
            response.body(tile.data)
        })
    }

    pub async fn get_tile_content(&self, xyz: TileCoord) -> ActixResult<Tile> {
        let mut tiles = try_join_all(self.sources.iter().map(|s| async {
            get_or_insert_cached_value!(
                self.cache,
                CacheValue::Tile,
                s.get_tile(xyz, self.query_obj.as_ref()),
                {
                    let id = s.get_id().to_owned();
                    if let Some(query_str) = self.query_str {
                        CacheKey::TileWithQuery(id, xyz, query_str.to_owned())
                    } else {
                        CacheKey::Tile(id, xyz)
                    }
                }
            )
        }))
        .await
        .map_err(map_internal_error)?;

        let mut layer_count = 0;
        let mut last_non_empty_layer = 0;
        for (idx, tile) in tiles.iter().enumerate() {
            if !tile.is_empty() {
                layer_count += 1;
                last_non_empty_layer = idx;
            }
        }

        // Minor optimization to prevent concatenation if there are less than 2 tiles
        let data = match layer_count {
            1 => tiles.swap_remove(last_non_empty_layer),
            0 => return Ok(Tile::new(Vec::new(), self.info)),
            _ => {
                // Make sure tiles can be concatenated, or if not, that there is only one non-empty tile for each zoom level
                // TODO: can zlib, brotli, or zstd be concatenated?
                // TODO: implement decompression step for other concatenate-able formats
                let can_join = self.info.format == Format::Mvt
                    && (self.info.encoding == Encoding::Uncompressed
                        || self.info.encoding == Encoding::Gzip);
                if !can_join {
                    return Err(ErrorBadRequest(format!(
                        "Can't merge {} tiles. Make sure there is only one non-empty tile source at zoom level {}",
                        self.info,
                        xyz.z
                    )))?;
                }
                tiles.concat()
            }
        };

        // decide if (re-)encoding of the tile data is needed, and recompress if so
        self.recompress(data)
    }

    fn recompress(&self, tile: TileData) -> ActixResult<Tile> {
        let mut tile = Tile::new(tile, self.info);
        if let Some(accept_enc) = &self.accept_enc {
            if self.info.encoding.is_encoded() {
                // already compressed, see if we can send it as is, or need to re-compress
                if !accept_enc.iter().any(|e| {
                    if let Preference::Specific(HeaderEnc::Known(enc)) = e.item {
                        to_encoding(enc) == Some(tile.info.encoding)
                    } else {
                        false
                    }
                }) {
                    // need to re-compress the tile - uncompress it first
                    tile = decode(tile)?;
                }
            }

            if tile.info.encoding == Encoding::Uncompressed {
                // let ordered_encodings = match self.preferred_enc {
                //     Some(PreferredEncoding::Gzip) | None => PREFER_GZIP_ENC,
                //     Some(PreferredEncoding::Brotli) => PREFER_BROTLI_ENC,
                // };
                let prefer = match self.preferred_enc {
                    Some(PreferredEncoding::Gzip) | None => PreferredEncoding::Gzip,
                    Some(PreferredEncoding::Brotli) => PreferredEncoding::Brotli,
                };

                // only apply compression if the content supports it
                if let Some(HeaderEnc::Known(enc)) = negotiate(accept_enc, prefer) {
                    // (re-)compress the tile into the preferred encoding
                    tile = encode(tile, enc)?;
                }
            }
            Ok(tile)
        } else {
            // no accepted-encoding header, decode the tile if compressed
            decode(tile)
        }
    }
}

fn negotiate(accept_enc: &AcceptEncoding, preferred_enc: PreferredEncoding) -> Option<HeaderEnc> {
    if accept_enc.is_empty() {
        // client can accept any(identity included)
        return match preferred_enc {
            PreferredEncoding::Brotli => Some(HeaderEnc::brotli()),
            PreferredEncoding::Gzip => Some(HeaderEnc::gzip()),
        };
    };
    // maybe client should just send a request withou accept-encoding header????
    if accept_enc.0[0].item.is_any() && accept_enc.0.len() == 1 {
        // client can accept any(identity included)
        return match preferred_enc {
            PreferredEncoding::Brotli => Some(HeaderEnc::brotli()),
            PreferredEncoding::Gzip => Some(HeaderEnc::gzip()),
        };
    }

    let acceptable_items = ranked_accept_items(accept_enc, preferred_enc).collect::<Vec<_>>();
    let identity_acceptable = is_identity_acceptable(&acceptable_items);

    let matched = acceptable_items
        .into_iter()
        .filter(|q| q.quality > Quality::ZERO)
        // search relies on item list being in descending order of quality
        .find(|q| {
            let enc = &q.item;
            matches!(enc, Preference::Specific(enc) if SUPPORTED_ENCODINGS.contains(enc))
        })
        .map(|q| q.item);
    match matched {
        Some(Preference::Specific(enc)) => Some(enc),

        _ if identity_acceptable => Some(ActixEncoding::identity()),

        _ => None,
    }
}
/// Returns true if "identity" is an acceptable encoding.
///
/// Internal algorithm relies on item list being in descending order of quality.
fn is_identity_acceptable(items: &'_ [QualityItem<Preference<ActixEncoding>>]) -> bool {
    if items.is_empty() {
        return true;
    }

    // Loop algorithm depends on items being sorted in descending order of quality. As such, it
    // is sufficient to return (q > 0) when reaching either an "identity" or "*" item.
    for q in items {
        match (q.quality, &q.item) {
            // occurrence of "identity;q=n"; return true if quality is non-zero
            (q, Preference::Specific(ActixEncoding::Known(ContentEncoding::Identity))) => {
                return q > Quality::ZERO
            }

            // occurrence of "*;q=n"; return true if quality is non-zero
            (q, Preference::Any) => return q > Quality::ZERO,

            _ => {}
        }
    }

    // implicit acceptable identity
    true
}
fn ranked_accept_items(
    accept_enc: &AcceptEncoding,
    prefer_encoding: PreferredEncoding,
) -> impl Iterator<Item = QualityItem<Preference<ActixEncoding>>> {
    if accept_enc.0.is_empty() {
        return Vec::new().into_iter();
    }

    let mut types = accept_enc.0.clone();

    // use stable sort so items with equal q-factor retain listed order
    types.sort_by(|a, b| {
        // sort by q-factor descending then server ranking descending

        b.quality
            .cmp(&a.quality)
            .then(encoding_rank(b, prefer_encoding).cmp(&encoding_rank(a, prefer_encoding)))
    });

    types.into_iter()
}
fn encoding_rank(
    qv: &QualityItem<Preference<ActixEncoding>>,
    prefer_encoding: PreferredEncoding,
) -> u8 {
    if qv.quality == Quality::ZERO {
        return 0;
    }
    match prefer_encoding {
        PreferredEncoding::Gzip => match qv.item {
            Preference::Specific(ActixEncoding::Known(ContentEncoding::Gzip)) => 5,
            Preference::Specific(ActixEncoding::Known(ContentEncoding::Brotli)) => 4,
            Preference::Specific(ActixEncoding::Known(ContentEncoding::Zstd)) => 3,
            Preference::Specific(ActixEncoding::Known(ContentEncoding::Deflate)) => 2,
            Preference::Any => 0,
            Preference::Specific(ActixEncoding::Known(ContentEncoding::Identity)) => 0,
            Preference::Specific(ActixEncoding::Known(_)) => 1,
            Preference::Specific(ActixEncoding::Unknown(_)) => 1,
        },
        PreferredEncoding::Brotli => match qv.item {
            Preference::Specific(ActixEncoding::Known(ContentEncoding::Brotli)) => 5,
            Preference::Specific(ActixEncoding::Known(ContentEncoding::Gzip)) => 4,
            Preference::Specific(ActixEncoding::Known(ContentEncoding::Zstd)) => 3,
            Preference::Specific(ActixEncoding::Known(ContentEncoding::Deflate)) => 2,
            Preference::Any => 0,
            Preference::Specific(ActixEncoding::Known(ContentEncoding::Identity)) => 0,
            Preference::Specific(ActixEncoding::Known(_)) => 1,
            Preference::Specific(ActixEncoding::Unknown(_)) => 1,
        },
    }
}

fn encode(tile: Tile, enc: ContentEncoding) -> ActixResult<Tile> {
    Ok(match enc {
        ContentEncoding::Brotli => Tile::new(
            encode_brotli(&tile.data)?,
            tile.info.encoding(Encoding::Brotli),
        ),
        ContentEncoding::Gzip => {
            Tile::new(encode_gzip(&tile.data)?, tile.info.encoding(Encoding::Gzip))
        }
        _ => tile,
    })
}

fn decode(tile: Tile) -> ActixResult<Tile> {
    let info = tile.info;
    Ok(if info.encoding.is_encoded() {
        match info.encoding {
            Encoding::Gzip => Tile::new(
                decode_gzip(&tile.data)?,
                info.encoding(Encoding::Uncompressed),
            ),
            Encoding::Brotli => Tile::new(
                decode_brotli(&tile.data)?,
                info.encoding(Encoding::Uncompressed),
            ),
            _ => Err(ErrorBadRequest(format!(
                "Tile is is stored as {info}, but the client does not accept this encoding"
            )))?,
        }
    } else {
        tile
    })
}

pub fn to_encoding(val: ContentEncoding) -> Option<Encoding> {
    Some(match val {
        ContentEncoding::Identity => Encoding::Uncompressed,
        ContentEncoding::Gzip => Encoding::Gzip,
        ContentEncoding::Brotli => Encoding::Brotli,
        // TODO: Deflate => Encoding::Zstd or Encoding::Zlib ?
        _ => None?,
    })
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use tilejson::tilejson;

    use super::*;
    use crate::srv::server::tests::TestSource;

    #[rstest]
    #[trace]
    // maybe client should just send a request withou accept-encoding header????
    #[case(&["*"],None,Encoding::Gzip)]
    #[case(&["*"],Some(PreferredEncoding::Brotli), Encoding::Brotli)]
    #[case(&["*"],Some(PreferredEncoding::Gzip), Encoding::Gzip)]
    #[case(&["gzip", "deflate", "br", "zstd"], None, Encoding::Gzip)]
    #[case(&["gzip", "deflate", "br", "zstd"], Some(PreferredEncoding::Brotli), Encoding::Brotli)]
    #[case(&["gzip", "deflate", "br", "zstd"], Some(PreferredEncoding::Gzip), Encoding::Gzip)]
    #[case(&["br;q=1", "gzip;q=1"], Some(PreferredEncoding::Gzip), Encoding::Gzip)]
    #[case(&["gzip;q=1", "br;q=1"], Some(PreferredEncoding::Brotli), Encoding::Brotli)]
    #[case(&["gzip;q=1", "br;q=0.5"], Some(PreferredEncoding::Brotli), Encoding::Gzip)]
    #[actix_rt::test]
    async fn test_enc_preference(
        #[case] accept_enc: &[&'static str],
        #[case] preferred_enc: Option<PreferredEncoding>,
        #[case] expected_enc: Encoding,
    ) {
        let sources = TileSources::new(vec![vec![Box::new(TestSource {
            id: "test_source",
            tj: tilejson! { tiles: vec![] },
            data: vec![1_u8, 2, 3],
        })]]);

        let accept_enc = Some(AcceptEncoding(
            accept_enc.iter().map(|s| s.parse().unwrap()).collect(),
        ));

        let src = DynTileSource::new(
            &sources,
            "test_source",
            None,
            "",
            accept_enc,
            preferred_enc,
            None,
        )
        .unwrap();

        let xyz = TileCoord { z: 0, x: 0, y: 0 };
        let tile = src.get_tile_content(xyz).await.unwrap();
        assert_eq!(tile.info.encoding, expected_enc);
    }

    #[actix_rt::test]
    async fn test_tile_content() {
        let non_empty_source = TestSource {
            id: "non-empty",
            tj: tilejson! { tiles: vec![] },
            data: vec![1_u8, 2, 3],
        };
        let empty_source = TestSource {
            id: "empty",
            tj: tilejson! { tiles: vec![] },
            data: Vec::default(),
        };
        let sources = TileSources::new(vec![vec![
            Box::new(non_empty_source),
            Box::new(empty_source),
        ]]);

        for (source_id, expected) in &[
            ("non-empty", vec![1_u8, 2, 3]),
            ("empty", Vec::<u8>::new()),
            ("empty,empty", Vec::<u8>::new()),
            ("non-empty,non-empty", vec![1_u8, 2, 3, 1_u8, 2, 3]),
            ("non-empty,empty", vec![1_u8, 2, 3]),
            ("non-empty,empty,non-empty", vec![1_u8, 2, 3, 1_u8, 2, 3]),
            ("empty,non-empty", vec![1_u8, 2, 3]),
            ("empty,non-empty,empty", vec![1_u8, 2, 3]),
        ] {
            let src = DynTileSource::new(&sources, source_id, None, "", None, None, None).unwrap();
            let xyz = TileCoord { z: 0, x: 0, y: 0 };
            assert_eq!(expected, &src.get_tile_content(xyz).await.unwrap().data);
        }
    }
}
