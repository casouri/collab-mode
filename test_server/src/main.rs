use std::collections::HashMap;

use anyhow::Context;
use collab_mode::{
    config_man,
    file_server::{self, Project},
};
use serde::Serialize;
use tiny_http::{Method, Request, Response, Server, StatusCode};
use url::{ParseError, Url};

fn send_400_missing(param_name: &str, request: Request) -> anyhow::Result<()> {
    request.respond(Response::new(
        400.into(),
        vec![],
        format!("{{\"message\": \"Missing param `{}`\"}}", param_name).as_bytes(),
        None,
        None,
    ))?;
    Ok(())
}

fn send_400_error(message: &str, request: Request) -> anyhow::Result<()> {
    request.respond(Response::new(
        400.into(),
        vec![],
        format!("{{\"message\": \"{}\"}}", message).as_bytes(),
        None,
        None,
    ))?;
    Ok(())
}

/// Respond to `request` with 200 if `resp` is `Ok`; respond with 500
/// otherwise.
fn respond_200_or_500<E: std::fmt::Debug>(
    resp: Result<String, E>,
    request: Request,
) -> anyhow::Result<()> {
    match resp {
        Ok(val) => {
            request.respond(Response::new(
                200.into(),
                vec![],
                val.as_bytes(),
                None,
                None,
            ))?;
        }
        Err(err) => {
            request.respond(Response::new(
                500.into(),
                vec![],
                format!("{{\"error\": \"{:?}\"}}", err).as_bytes(),
                None,
                None,
            ))?;
        }
    }
    Ok(())
}

pub fn main() -> Result<(), anyhow::Error> {
    let server = Server::http("127.0.0.1:8000").unwrap();

    let conn = rusqlite::Connection::open_in_memory()?;

    file_server::init_db(&conn)?;

    for request in server.incoming_requests() {
        println!(
            "received request! method: {:?}, url: {:?}, headers: {:?}",
            request.method(),
            request.url(),
            request.headers()
        );

        let url = Url::parse(&format!("http://127.0.0.1:8000{}", request.url()));
        if url.is_err() {
            send_400_error("Can't parse URL", request)?;
            continue;
        }

        let url = url.unwrap();

        let queries: HashMap<String, String> = url
            .query_pairs()
            .into_iter()
            .map(|(key, val)| (key.into_owned(), val.into_owned()))
            .collect();

        if url.path() == "/projects" {
            let resp =
                file_server::list_projects(&conn).map(|resp| serde_json::to_string(&resp).unwrap());
            respond_200_or_500(resp, request)?;
            continue;
        }

        if url.path() == "/add-project" {
            let path = queries.get("path");
            if path.is_none() {
                send_400_missing("path", request)?;
                continue;
            }

            let path = path.unwrap();
            let proj = file_server::add_project(path, &conn)
                .map(|proj| serde_json::to_string(&proj).unwrap());
            respond_200_or_500(proj, request)?;
            continue;
        }

        if url.path() == "/list-files" {
            let project = queries.get("project");
            if project.is_none() {
                send_400_missing("project", request)?;
                continue;
            }

            let project = str::parse::<u64>(project.unwrap());
            if project.is_err() {
                send_400_error("Can't parse project", request)?;
                continue;
            }
            let project = project.unwrap();

            let path = queries.get("path");
            if path.is_none() {
                send_400_missing("path", request)?;
                continue;
            }
            let path = path.unwrap();

            let files = file_server::list_files(project, path, &conn)
                .map(|files| serde_json::to_string(&files).unwrap());
            respond_200_or_500(files, request)?;
            continue;
        }

        if url.path() == "/file-content" {
            let project = queries.get("project");
            if project.is_none() {
                send_400_missing("project", request)?;
                continue;
            }

            let project = str::parse::<u64>(project.unwrap());
            if project.is_err() {
                send_400_error("Can't parse project", request)?;
                continue;
            }
            let project = project.unwrap();

            let path = queries.get("path");
            if path.is_none() {
                send_400_missing("path", request)?;
                continue;
            }
            let path = path.unwrap();

            let content = file_server::file_content(project, path, &conn)
                .map(|content| serde_json::to_string(&content).unwrap());
            respond_200_or_500(content, request)?;
            continue;
        }

        let response = Response::from_string("hello world");
        request.respond(response).unwrap();
    }

    Ok(())
}
