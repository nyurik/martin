use std::cmp::Ordering;
use std::string::ToString;
use std::time::Duration;

use actix_cors::Cors;
use actix_http::ContentEncoding;
use actix_web::dev::Server;
use actix_web::error::ErrorBadRequest;
use actix_web::http::header::{
    AcceptEncoding, Encoding as HeaderEnc, HeaderValue, Preference, CACHE_CONTROL, CONTENT_ENCODING,
};
use actix_web::http::Uri;
use actix_web::middleware::TrailingSlash;
use actix_web::web::{Data, Path, Query};
use actix_web::{
    error, middleware, route, web, App, Error, HttpMessage, HttpRequest, HttpResponse, HttpServer,
    Responder, Result,
};
use futures::future::try_join_all;
use itertools::Itertools;
use log::{debug, error};
use martin_tile_utils::{Encoding, Format, TileInfo};
use serde::{Deserialize, Serialize};
use tilejson::TileJSON;

use crate::source::{Source, Sources, UrlQuery, Xyz};
use crate::srv::config::{SrvConfig, KEEP_ALIVE_DEFAULT, LISTEN_ADDRESSES_DEFAULT};
use crate::utils::{decode_brotli, decode_gzip, encode_brotli, encode_gzip};
use crate::Error::BindingError;

/// List of keywords that cannot be used as source IDs. Some of these are reserved for future use.
/// Reserved keywords must never end in a "dot number" (e.g. ".1")
pub const RESERVED_KEYWORDS: &[&str] = &[
    "catalog", "config", "health", "help", "index", "manifest", "refresh", "reload", "status",
];

static SUPPORTED_ENCODINGS: &[HeaderEnc] = &[
    HeaderEnc::brotli(),
    HeaderEnc::gzip(),
    HeaderEnc::identity(),
];

pub struct AppState {
    pub sources: Sources,
}

impl AppState {
    fn get_source(&self, id: &str) -> Result<&dyn Source> {
        Ok(self
            .sources
            .get(id)
            .ok_or_else(|| error::ErrorNotFound(format!("Source {id} does not exist")))?
            .as_ref())
    }

    fn get_sources(
        &self,
        source_ids: &str,
        zoom: Option<u8>,
    ) -> Result<(Vec<&dyn Source>, bool, TileInfo)> {
        let mut sources = Vec::new();
        let mut info: Option<TileInfo> = None;
        let mut use_url_query = false;
        for id in source_ids.split(',') {
            let src = self.get_source(id)?;
            let src_inf = src.get_tile_info();
            use_url_query |= src.support_url_query();

            // make sure all sources have the same format
            match info {
                Some(inf) if inf == src_inf => {}
                Some(inf) => Err(error::ErrorNotFound(format!(
                    "Cannot merge sources with {inf} with {src_inf}"
                )))?,
                None => info = Some(src_inf),
            }

            // TODO: Use chained-if-let once available
            if match zoom {
                Some(zoom) if check_zoom(src, id, zoom) => true,
                None => true,
                _ => false,
            } {
                sources.push(src);
            }
        }

        // format is guaranteed to be Some() here
        Ok((sources, use_url_query, info.unwrap()))
    }
}

#[derive(Deserialize)]
struct TileJsonRequest {
    source_ids: String,
}

#[derive(Deserialize)]
struct TileRequest {
    source_ids: String,
    z: u8,
    x: u32,
    y: u32,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct IndexEntry {
    pub id: String,
    pub content_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_encoding: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attribution: Option<String>,
}

impl PartialOrd<Self> for IndexEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for IndexEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        (&self.id, &self.name).cmp(&(&other.id, &other.name))
    }
}

fn map_internal_error<T: std::fmt::Display>(e: T) -> Error {
    error!("{e}");
    error::ErrorInternalServerError(e.to_string())
}

/// Root path will eventually have a web front. For now, just a stub.
#[route("/", method = "GET", method = "HEAD")]
#[allow(clippy::unused_async)]
async fn get_index() -> &'static str {
    "Martin server is running. Eventually this will be a nice web front.\n\n\
    A list of all available sources is at /catalog\n\n\
    See documentation https://github.com/maplibre/martin"
}

/// Return 200 OK if healthy. Used for readiness and liveness probes.
#[route("/health", method = "GET", method = "HEAD")]
#[allow(clippy::unused_async)]
async fn get_health() -> impl Responder {
    HttpResponse::Ok()
        .insert_header((CACHE_CONTROL, "no-cache"))
        .message_body("OK")
}

#[route(
    "/catalog",
    method = "GET",
    method = "HEAD",
    wrap = "middleware::Compress::default()"
)]
#[allow(clippy::unused_async)]
async fn get_catalog(state: Data<AppState>) -> impl Responder {
    let info: Vec<_> = state
        .sources
        .iter()
        .map(|(id, src)| {
            let tilejson = src.get_tilejson();
            let info = src.get_tile_info();
            IndexEntry {
                id: id.clone(),
                content_type: info.format.content_type().to_string(),
                content_encoding: info.encoding.content_encoding().map(ToString::to_string),
                name: tilejson.name.filter(|v| v != id),
                description: tilejson.description,
                attribution: tilejson.attribution,
            }
        })
        .sorted()
        .collect();
    HttpResponse::Ok().json(info)
}

#[route(
    "/{source_ids}",
    method = "GET",
    method = "HEAD",
    wrap = "middleware::Compress::default()"
)]
#[allow(clippy::unused_async)]
async fn git_source_info(
    req: HttpRequest,
    path: Path<TileJsonRequest>,
    state: Data<AppState>,
) -> Result<HttpResponse> {
    let sources = state.get_sources(&path.source_ids, None)?.0;

    let tiles_path = req
        .headers()
        .get("x-rewrite-url")
        .and_then(parse_x_rewrite_url)
        .unwrap_or_else(|| req.path().to_owned());

    let info = req.connection_info();
    let tiles_url = get_tiles_url(info.scheme(), info.host(), req.query_string(), &tiles_path)?;

    Ok(HttpResponse::Ok().json(merge_tilejson(sources, tiles_url)))
}

fn get_tiles_url(scheme: &str, host: &str, query_string: &str, tiles_path: &str) -> Result<String> {
    let path_and_query = if query_string.is_empty() {
        format!("{tiles_path}/{{z}}/{{x}}/{{y}}")
    } else {
        format!("{tiles_path}/{{z}}/{{x}}/{{y}}?{query_string}")
    };

    Uri::builder()
        .scheme(scheme)
        .authority(host)
        .path_and_query(path_and_query)
        .build()
        .map(|tiles_url| tiles_url.to_string())
        .map_err(|e| ErrorBadRequest(format!("Can't build tiles URL: {e}")))
}

fn merge_tilejson(sources: Vec<&dyn Source>, tiles_url: String) -> TileJSON {
    let mut tilejson = sources
        .into_iter()
        .map(Source::get_tilejson)
        .reduce(|mut accum, tj| {
            if let Some(minzoom) = tj.minzoom {
                if let Some(a) = accum.minzoom {
                    if a > minzoom {
                        accum.minzoom = tj.minzoom;
                    }
                } else {
                    accum.minzoom = tj.minzoom;
                }
            }
            if let Some(maxzoom) = tj.maxzoom {
                if let Some(a) = accum.maxzoom {
                    if a < maxzoom {
                        accum.maxzoom = tj.maxzoom;
                    }
                } else {
                    accum.maxzoom = tj.maxzoom;
                }
            }
            if let Some(bounds) = tj.bounds {
                if let Some(a) = accum.bounds {
                    accum.bounds = Some(a + bounds);
                } else {
                    accum.bounds = tj.bounds;
                }
            }

            accum
        })
        .expect("nonempty sources iter");

    tilejson.tiles.push(tiles_url);
    tilejson
}

#[route("/{source_ids}/{z}/{x}/{y}", method = "GET", method = "HEAD")]
async fn get_tile(
    req: HttpRequest,
    path: Path<TileRequest>,
    state: Data<AppState>,
) -> Result<HttpResponse> {
    let xyz = Xyz {
        z: path.z,
        x: path.x,
        y: path.y,
    };

    // Optimization for a single-source request.
    let (tile, info) = if path.source_ids.contains(',') {
        let (sources, use_url_query, info) = state.get_sources(&path.source_ids, Some(path.z))?;
        if sources.is_empty() {
            return Err(error::ErrorNotFound("No valid sources found"))?;
        }
        let query = if use_url_query {
            Some(Query::<UrlQuery>::from_query(req.query_string())?.into_inner())
        } else {
            None
        };
        let tiles = try_join_all(sources.into_iter().map(|s| s.get_tile(&xyz, &query)))
            .await
            .map_err(map_internal_error)?;
        // Make sure tiles can be concatenated, or if not, that there is only one non-empty tile for each zoom level
        // TODO: can zlib, brotli, or zstd be concatenated?
        // TODO: implement decompression step for other concatenate-able formats
        let can_join = info.format == Format::Mvt
            && (info.encoding == Encoding::Uncompressed || info.encoding == Encoding::Gzip);
        if !can_join && tiles.iter().filter(|v| !v.is_empty()).count() > 1 {
            return Err(error::ErrorBadRequest(format!(
                "Can't merge {info} tiles. Make sure there is only one non-empty tile source at zoom level {}",
                xyz.z
            )))?;
        }
        (tiles.concat(), info)
    } else {
        let id = &path.source_ids;
        let zoom = xyz.z;
        let src = state.get_source(id)?;
        if !check_zoom(src, id, zoom) {
            return Err(error::ErrorNotFound(format!(
                "Zoom {zoom} is not valid for source {id}",
            )));
        }
        let query = if src.support_url_query() {
            Some(Query::<UrlQuery>::from_query(req.query_string())?.into_inner())
        } else {
            None
        };
        let tile = src
            .get_tile(&xyz, &query)
            .await
            .map_err(map_internal_error)?;
        (tile, src.get_tile_info())
    };

    Ok(if tile.is_empty() {
        HttpResponse::NoContent().finish()
    } else {
        // decide if (re-)encoding of the tile data is needed, and recompress if so
        let (tile, info) = recompress(tile, info, req.get_header::<AcceptEncoding>())?;
        let mut response = HttpResponse::Ok();
        response.content_type(info.format.content_type());
        if let Some(val) = info.encoding.content_encoding() {
            response.insert_header((CONTENT_ENCODING, val));
        }
        response.body(tile)
    })
}

fn recompress(
    mut tile: Vec<u8>,
    mut info: TileInfo,
    accept_enc: Option<AcceptEncoding>,
) -> Result<(Vec<u8>, TileInfo)> {
    if let Some(accept_enc) = accept_enc {
        if info.encoding.is_encoded() {
            // already compressed, see if we can send it as is, or need to re-compress
            if !accept_enc.iter().any(|e| {
                if let Preference::Specific(HeaderEnc::Known(enc)) = e.item {
                    to_encoding(enc) == Some(info.encoding)
                } else {
                    false
                }
            }) {
                // need to re-compress the tile - uncompress it first
                (tile, info) = decode(tile, info)?;
            }
        }
        if info.encoding == Encoding::Uncompressed {
            // only apply compression if the content supports it
            if let Some(HeaderEnc::Known(enc)) = accept_enc.negotiate(SUPPORTED_ENCODINGS.iter()) {
                // (re-)compress the tile into the preferred encoding
                (tile, info) = encode(tile, info, enc)?;
            }
        }
        Ok((tile, info))
    } else {
        // no accepted-encoding header, decode the tile if compressed
        decode(tile, info)
    }
}

fn encode(tile: Vec<u8>, info: TileInfo, enc: ContentEncoding) -> Result<(Vec<u8>, TileInfo)> {
    Ok(match enc {
        ContentEncoding::Brotli => (encode_brotli(&tile)?, info.encoding(Encoding::Brotli)),
        ContentEncoding::Gzip => (encode_gzip(&tile)?, info.encoding(Encoding::Gzip)),
        _ => (tile, info),
    })
}

fn decode(tile: Vec<u8>, info: TileInfo) -> Result<(Vec<u8>, TileInfo)> {
    Ok(if info.encoding.is_encoded() {
        match info.encoding {
            Encoding::Gzip => (decode_gzip(&tile)?, info.encoding(Encoding::Uncompressed)),
            Encoding::Brotli => (decode_brotli(&tile)?, info.encoding(Encoding::Uncompressed)),
            _ => Err(ErrorBadRequest(format!(
                "Tile is is stored as {info}, but the client does not accept this encoding"
            )))?,
        }
    } else {
        (tile, info)
    })
}

fn to_encoding(val: ContentEncoding) -> Option<Encoding> {
    Some(match val {
        ContentEncoding::Identity => Encoding::Uncompressed,
        ContentEncoding::Gzip => Encoding::Gzip,
        ContentEncoding::Brotli => Encoding::Brotli,
        // TODO: Deflate => Encoding::Zstd or Encoding::Zlib ?
        _ => None?,
    })
}

pub fn router(cfg: &mut web::ServiceConfig) {
    cfg.service(configure_files())
        .service(get_health)
        .service(get_index)
        .service(get_catalog)
        .service(git_source_info)
        .service(get_tile);
}

/// Create a new initialized Actix `App` instance together with the listening address.
pub fn new_server(config: SrvConfig, sources: Sources) -> crate::Result<(Server, String)> {
    let keep_alive = Duration::from_secs(config.keep_alive.unwrap_or(KEEP_ALIVE_DEFAULT));
    let worker_processes = config.worker_processes.unwrap_or_else(num_cpus::get);
    let listen_addresses = config
        .listen_addresses
        .unwrap_or_else(|| LISTEN_ADDRESSES_DEFAULT.to_owned());

    let server = HttpServer::new(move || {
        let state = AppState {
            sources: sources.clone(),
        };

        let cors_middleware = Cors::default()
            .allow_any_origin()
            .allowed_methods(vec!["GET"]);

        App::new()
            .app_data(Data::new(state))
            .wrap(cors_middleware)
            .wrap(middleware::NormalizePath::new(TrailingSlash::MergeOnly))
            .wrap(middleware::Logger::default())
            .configure(router)
    })
    .bind(listen_addresses.clone())
    .map_err(|e| BindingError(e, listen_addresses.clone()))?
    .keep_alive(keep_alive)
    .shutdown_timeout(0)
    .workers(worker_processes)
    .run();

    Ok((server, listen_addresses))
}

#[must_use]
pub fn configure_files() -> actix_files::Files {
    actix_files::Files::new("/maputnik", "./maputnik")
        .redirect_to_slash_directory()
        .index_file("index.html")
}

fn check_zoom(src: &dyn Source, id: &str, zoom: u8) -> bool {
    let is_valid = src.is_valid_zoom(zoom);
    if !is_valid {
        debug!("Zoom {zoom} is not valid for source {id}");
    }
    is_valid
}

fn parse_x_rewrite_url(header: &HeaderValue) -> Option<String> {
    header
        .to_str()
        .ok()
        .and_then(|header| header.parse::<Uri>().ok())
        .map(|uri| uri.path().to_owned())
}
