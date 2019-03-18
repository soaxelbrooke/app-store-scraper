
# App Store Scraper

## Run

Running the scraper with no args will cause it to run for all app IDs discovered via top rankings:

```
$ cargo run
```

You can request it scrape specific apps as well by including them in the `SCRAPE_APP_IDS` environment variable:

```
$ SCRAPE_APP_IDS=389801252,951627425 cargo run
```
