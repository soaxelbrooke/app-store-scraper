#[macro_use] extern crate log;
#[macro_use] extern crate lazy_static;

extern crate env_logger;
extern crate chrono;
extern crate rusqlite;
extern crate reqwest;
extern crate xmltree;
extern crate backoff;
extern crate prometheus;
extern crate hyper;

use chrono::prelude::*;
use backoff::{Error, ExponentialBackoff, Operation, backoff::Backoff};
use reqwest::header::USER_AGENT;
use rusqlite::{Connection, NO_PARAMS};
use rusqlite::Error::SqliteFailure;
use xmltree::Element;
use std::io::{Cursor, Read};
use std::{thread, time, str, process};
use clap::{Arg, App};
use prometheus::{Counter, Opts, TextEncoder, Encoder, register_counter};
use hyper::rt::Future;
use hyper::service::service_fn_ok;
use hyper::{Body, Request, Response, Server};

const CHROME_USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/70.0.3538.77 Safari/537.36";
const APP_CATEGORIES: &'static [i64] = &[
    6000,
    6001,
    6002,
    6003,
    6004,
    6005,
    6006,
    6007,
    6008,
    6009,
    6010,
    6011,
    6012,
    6013,
    6014,
    7001,
    7002,
    7003,
    7004,
    7005,
    7006,
    7007,
    7008,
    7009,
    7010,
    7011,
    7012,
    7013,
    7014,
    7015,
    7016,
    7017,
    7018,
    7019,
    6015,
    6016,
    6017,
    6018,
    6019,
    6020,
    6021,
    13001,
    13002,
    13003,
    13004,
    13005,
    13006,
    13007,
    13008,
    13009,
    13011,
    13012,
    13013,
    13014,
    13015,
    13016,
    13017,
    13018,
    13019,
    13020,
    13021,
    13022,
    13023,
    13024,
    13025,
    13026,
    13027,
    13028,
    13029,
    13030,
    6022,
];
const METRICS_PORT: u16 = 9803;

lazy_static! {
    static ref APP_SCRAPES: Counter = register_counter!(Opts::new(
        "ios_app_scrape_count",
        "Count of iOS apps scraped",
    )).unwrap();
    static ref REVIEW_SCRAPES: Counter = register_counter!(Opts::new(
        "ios_review_scrape_count",
        "Count of iOS reviews scraped",
    )).unwrap();
}

#[derive(Debug)]
struct AppVersion {
    app_id: String,
    updated_at: DateTime<Utc>,
    category: String,
    name: String,
    publisher: String,
    summary: String,
    released_at: DateTime<Utc>,
    inserted_at: DateTime<Utc>,
    xml_raw: String,
}

#[derive(Debug)]
struct Review {
    review_id: String,
    app_id: String,
    title: String,
    body: String,
    rating: i64,
    version: String,
    updated_at: DateTime<Utc>,
    author_name: String,
    inserted_at: DateTime<Utc>,
    xml_raw: String,
}

fn maybe_create_db() -> rusqlite::Result<Connection> {
    let conn = Connection::open("database.sqlite")?;

    conn.execute(r#"
        create table if not exists apps (
            app_id text not null primary key,
            updated_at text,
            category text not null,
            name text not null,
            publisher text not null, -- From the `artist` field
            summary text not null,
            released_at text not null,
            inserted_at text not null,
            xml_raw text not null
        );
    "#, NO_PARAMS)?;

    conn.execute(r#"
        create table if not exists reviews (
            review_id text not null primary key,
            app_id text not null,
            title text not null,
            body text not null, -- content[type="text"]
            rating integer not null,
            version text not null,
            updated_at text not null,
            author_name text not null,
            inserted_at text not null,
            xml_raw text not null
        );
    "#, NO_PARAMS)?;

    Ok(conn)
}

fn fetch_url(url: &str) -> Result<String, Error<reqwest::Error>> {
    debug!("Requesting URL {}", url);
    thread::sleep(time::Duration::from_millis(100));
    let mut op = || {
        debug!("Fetching {}", url);
        let client = reqwest::Client::new();
        let mut resp = client.get(url).header(USER_AGENT, CHROME_USER_AGENT).send()?;
        Ok(resp.text()?)
    };

    let mut backoff = ExponentialBackoff::default();
    op.retry(&mut backoff)
}

fn log_and_erase_err<T: Clone, E>(to_unwrap: &Result<T, E>, log_message: &str) -> Result<T, ()> {
    match to_unwrap {
        Err(_err) => {
            error!("{}", log_message);
            Err(())
        },
        Ok(t) => Ok(t.clone())
    }
}

fn log_empty_and_err<T: Clone>(to_unwrap: &Option<T>, log_message: &str) -> Result<T, ()> {
    match to_unwrap {
        Some(t) => Ok(t.clone()),
        None => {
            error!("{}", log_message);
            Err(())
        }
    }
}

fn get_node_or_err(node: &Element, name: &str) -> Result<Element, ()> {
    log_empty_and_err(&node.get_child(name).map(|n| n.clone()), &format!("Unable to find node with name {}", name))
}

fn get_text(node: &Element) -> Result<String, ()> {
    match node.text.clone() {
        Some(text) => Ok(text),
        None => {
            error!("No text found in {} node", node.name);
            Err(())
        }
    }
}

fn get_node_text(node: &Element, name: &str) -> Result<String, ()> {
    get_text(&get_node_or_err(node, name)?)
}

fn parse_date(date_str: &String) -> Result<DateTime<Utc>, ()> {
    log_and_erase_err(&DateTime::parse_from_rfc3339(date_str), "Couldn't parse date from string").map(|dt| dt.with_timezone(&Utc))
}

fn get_node_dt(node: &Element, name: &str) -> Result<DateTime<Utc>, ()> {
    parse_date(&get_node_text(node, name)?)
}

fn get_node_attr(node: &Element, name: &str, attr: &str) -> Result<String, ()> {
    get_node_or_err(node, name)?.attributes.get(attr).ok_or(()).map(|s| s.clone())
}

fn build_review (app_id: &String, node: &Element) -> Result<Review, ()> {
    let mut xml_curs: Cursor<Vec<u8>> = Cursor::new(Vec::new());
    log_and_erase_err(&node.write(&mut xml_curs), "Couldn't write XML to cursor")?;
    xml_curs.set_position(0);
    let mut xml_raw = Vec::new();
    log_and_erase_err(&xml_curs.read_to_end(&mut xml_raw), "Couldn't read cursor into string")?;
    let xml_string = String::from(log_and_erase_err(&str::from_utf8(&xml_raw), "Couldn't parse UTF8 string")?);

    let body = get_text(
        &log_empty_and_err(
            &node.children.iter()
                .filter(|node| node.name == "content" && node.attributes.get("type").unwrap_or(&"".to_string()) == "text")
                .next(),
            r#"No contet[type="text"] element found"#
        )?.clone()
    )?;

    Ok(
        Review {
            review_id: get_node_text(node, "id")?,
            app_id: app_id.clone(),
            title: get_node_text(node, "title")?,
            body: body,
            rating: log_and_erase_err(&get_node_text(node, "rating")?.parse::<i64>(), "Unable to parse rating as int")?,
            version: get_node_text(node, "version")?,
            updated_at: get_node_dt(node, "updated")?,
            author_name: get_node_text(&get_node_or_err(node, "author")?, "name")?,
            inserted_at: Utc::now(),
            xml_raw: xml_string,
        }
    )
}

fn parse_reviews(app_id: &String, xml: &String) -> Result<Vec<Review>, ()> {
    let document = log_and_erase_err(&Element::parse(xml.as_bytes()), "Couldn't parse XML from document")?;
    let mut results: Vec<Review> = Vec::new();
    for node in document.children.iter().filter(|node| node.name == "entry") {
        results.push(build_review(app_id, node)?);
    }

    Ok(results)
}

fn insert_review(conn: &Connection, review: &Review) -> Result<(), rusqlite::Error> {
    let params = &[
        &review.review_id,
        &review.app_id,
        &review.title,
        &review.body,
        &review.rating.to_string(),
        &review.version,
        &review.updated_at.to_rfc3339_opts(SecondsFormat::Millis, true),
        &review.author_name,
        &review.inserted_at.to_rfc3339_opts(SecondsFormat::Millis, true),
        &review.xml_raw,
    ];
    conn.execute(r#"
        insert into reviews (
            review_id,
            app_id,
            title,
            body,
            rating,
            version,
            updated_at,
            author_name,
            inserted_at,
            xml_raw
        ) values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10);
    "#, params)?;
    REVIEW_SCRAPES.inc();
    Ok(())
}

fn fetch_reviews(app_id: &String, page: &usize) -> Result<Vec<Review>, ()> {
    let mut backoff = ExponentialBackoff::default();
    while let Some(next_backoff) = backoff.next_backoff() {
        let resp = fetch_url(&format!("https://itunes.apple.com/us/rss/customerreviews/id={}/page={}/sortBy=mostRecent/xml", app_id, page).to_string());
        if let Ok(reviews) = parse_reviews(app_id, &log_and_erase_err(&resp, &format!("Unable to fetch reviews for app_id {} and page {}", app_id, page))?) {
            return Ok(reviews);
        } else {
            warn!("Failed to parse reviews, sleeping for {} seconds.", next_backoff.as_secs());
            thread::sleep(next_backoff);
        }
    }
    Err(())
}

fn pull_reviews_for_app_id(conn: &Connection, app_id: &String) -> Result<(), ()> {
    info!("Scraping reviews for app ID {}", app_id);
    for page in 1..10 {
        let reviews = fetch_reviews(app_id, &page)?;
        for review in reviews.iter() {
            let result = insert_review(conn, &review);
            if let Err(err) = result {
                if let SqliteFailure(result_code, _) = err {
                    if result_code.extended_code == 1555 {
                        debug!("Found duplicate review, stopping for this app.");
                        return Ok(());
                    } else {
                        error!("Unexpected sqlite error: {}", err);
                    }
                } else {
                    error!("Unexpected sqlite error: {}", err);
                }
            }
        }
    }
    Ok(())
}

fn pull_top_apps_for_category(conn: &Connection, category: &i64) -> Result<(), ()> {
    info!("Pulling apps for category {}", category);
    let resp = fetch_url(&format!("https://itunes.apple.com/us/rss/topgrossingapplications/limit=200/genre={}/xml", category).to_string());
    let apps = parse_apps(log_and_erase_err(&resp, &format!("Unable to fetch apps for category {}", category))?)?;

    for app in apps.iter() {
        let result = insert_app(conn, &app);
        if let Err(err) = result {
            if let SqliteFailure(result_code, _) = err {
                if result_code.extended_code == 1555 {
                    debug!("Ignoring duplicate app version for {}", app.app_id);
                } else {
                    error!("Unexpected sqlite error: {}", err);
                }
            } else {
                error!("Unexpected sqlite error: {}", err);
            }
        }
    }

    Ok(())
}

fn parse_apps(xml: String) -> Result<Vec<AppVersion>, ()> {
    let document = log_and_erase_err(&Element::parse(xml.as_bytes()), "Couldn't parse XML from document")?;
    let mut results: Vec<AppVersion> = Vec::new();

    for node in document.children.iter().filter(|node| node.name == "entry") {
        if let Err(_err) = build_app(node).map(|app| results.push(app)) {
            error!("Failed parsing app.");
        }
    }

    Ok(results)
}

fn build_app(node: &Element) -> Result<AppVersion, ()> {
    let mut xml_curs: Cursor<Vec<u8>> = Cursor::new(Vec::new());
    log_and_erase_err(&node.write(&mut xml_curs), "Couldn't write XML to cursor")?;
    xml_curs.set_position(0);
    let mut xml_raw = Vec::new();
    log_and_erase_err(&xml_curs.read_to_end(&mut xml_raw), "Couldn't read cursor into string")?;
    let xml_string = String::from(log_and_erase_err(&str::from_utf8(&xml_raw), "Couldn't parse UTF8 string")?);

    Ok(
        AppVersion {
            app_id: get_node_attr(node, "id", "id")?,
            category: get_node_attr(node, "category", "id")?,
            inserted_at: Utc::now(),
            name: get_node_text(node, "name")?,
            publisher: get_node_text(node, "artist")?,
            updated_at: get_node_dt(node, "updated")?,
            released_at: get_node_dt(node, "releaseDate")?,
            summary: get_node_text(node, "summary")?,
            xml_raw: xml_string,
        }
    )
}

fn insert_app(conn: &Connection, app: &AppVersion) -> Result<(), rusqlite::Error> {
    let params = &[
        &app.app_id,
        &app.updated_at.to_rfc3339_opts(SecondsFormat::Millis, true),
        &app.category,
        &app.name,
        &app.publisher,
        &app.summary,
        &app.released_at.to_rfc3339_opts(SecondsFormat::Millis, true),
        &app.inserted_at.to_rfc3339_opts(SecondsFormat::Millis, true),
        &app.xml_raw
    ];
    conn.execute(r#"
        insert into apps (
            app_id,
            updated_at,
            category,
            name,
            publisher,
            summary,
            released_at,
            inserted_at,
            xml_raw
        ) values ($1, $2, $3, $4, $5, $6, $7, $8, $9)
        on conflict (app_id) do update set
            updated_at=excluded.updated_at,
            category=excluded.category,
            name=excluded.name,
            publisher=excluded.publisher,
            summary=excluded.summary,
            released_at=excluded.released_at,
            inserted_at=excluded.inserted_at,
            xml_raw=excluded.xml_raw;
    "#, params)?;
    APP_SCRAPES.inc();
    Ok(())
}

fn get_all_app_ids(conn: &Connection) -> Vec<String> {
    let mut stmt = conn.prepare("select distinct(app_id) from apps").unwrap();
    stmt.query_map(NO_PARAMS, |row| {
        row.get(0)
    }).unwrap().map(|res| res.unwrap()).collect()
}

fn scrape_top_apps() {
    info!("Maybe creating database...");
    let result = maybe_create_db();
    if let Err(err) = result {
        error!("Failed to created database: {}", err);
        process::exit(1);
    }
    let conn = result.unwrap();
    for category in APP_CATEGORIES.iter() {
        pull_top_apps_for_category(&conn, &category).unwrap();
    }
    info!("Done!");
}

fn scrape_reviews(app_id_requested: Option<&str>) {
    info!("Maybe creating database...");
    let result = maybe_create_db();
    if let Err(err) = result {
        error!("Failed to created database: {}", err);
        process::exit(1);
    }
    let conn = result.unwrap();
    match app_id_requested {
        None => {
            for app_id in get_all_app_ids(&conn) {
                if let Err(_err) = pull_reviews_for_app_id(&conn, &app_id) {
                    error!("Failed to scrape reviews for app {}", app_id);
                }
            }
        },
        Some(app_id) => {
            if let Err(_err) = pull_reviews_for_app_id(&conn, &String::from(app_id)) {
                error!("Failed to scrape reviews for app {}", app_id);
            }
        }
    }
}

fn metric_service(_req: Request<Body>) -> Response<Body> {
    let encoder = TextEncoder::new();
    let mut buffer = vec![];
    let mf = prometheus::gather();
    encoder.encode(&mf, &mut buffer).unwrap();
    Response::builder()
        .header(hyper::header::CONTENT_TYPE, encoder.format_type())
        .body(Body::from(buffer))
        .unwrap()
}

fn run_metrics_server() {
    let addr = ([0, 0, 0, 0], METRICS_PORT).into();
    let service = || service_fn_ok(metric_service);
    let server = Server::bind(&addr)
        .serve(service)
        .map_err(|e| panic!("{}", e));

    hyper::rt::run(server);
}

fn main() {
    env_logger::init();
    let matches = App::new("app-store-scraper")
        .version("0.1")
        .arg(Arg::with_name("mode")
            .short("m")
            .long("mode")
            .takes_value(true)
            .help("Mode to run in ('apps' or 'reviews')"))
        .arg(Arg::with_name("app_id")
            .long("app-id")
            .takes_value(true)
            .help("A single app ID to pull reviews for"))
        .get_matches();
    
    let mode = matches.value_of("mode").unwrap_or("reviews");
    if mode == "apps" {
        std::thread::spawn(run_metrics_server);
        scrape_top_apps();
    } else if mode == "reviews" {
        let app_id_requested = matches.value_of("app_id");
        std::thread::spawn(run_metrics_server);
        scrape_reviews(app_id_requested);
    } else {
        error!("Didn't understand requested mode {}", mode);
    }
}
