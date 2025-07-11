use std::{convert::Infallible, net::SocketAddr, sync::Arc, time::Duration};

use async_stream::stream;
use bytes::Bytes;
use faststr::FastStr;
use futures::Stream;
use http::{Method, StatusCode, Uri, header};
use http_body::Frame;
use serde::{Deserialize, Serialize};
use tokio_stream::StreamExt;
use volo::{catch_panic, service::service_fn};
use volo_http::{
    Address,
    body::{Body, BodyConversion},
    context::ServerContext,
    request::Request,
    response::Response,
    server::{
        IntoResponse, Redirect, Server,
        extract::{Form, FullUri, Json, MaybeInvalid, Query},
        layer::{FilterLayer, TimeoutLayer},
        middleware::{self, Next},
        panic_handler::{always_internal_error, fixed_payload},
        param::PathParams,
        response::sse::{Event, Sse},
        route::{Router, get, get_service, post},
        utils::ServeDir,
    },
    utils::{
        Extension,
        cookie::{self, CookieJar},
    },
};

async fn hello() -> &'static str {
    "hello, world\n"
}

#[derive(Serialize, Deserialize, Debug)]
struct Person {
    name: String,
    age: u8,
    phones: Vec<String>,
}

async fn json_get() -> Json<Person> {
    Json(Person {
        name: "Foo".to_string(),
        age: 25,
        phones: vec!["Bar".to_string(), "114514".to_string()],
    })
}

async fn json_post(Json(request): Json<Person>) -> String {
    let first_phone = request
        .phones
        .first()
        .map(|p| p.as_str())
        .unwrap_or("no number");
    format!(
        "{} is {} years old, {}'s first phone number is `{}`\n",
        request.name, request.age, request.name, first_phone
    )
}

async fn json_post_with_check(request: Option<Json<Person>>) -> Result<String, StatusCode> {
    let request = match request {
        Some(Json(req)) => req,
        None => {
            return Err(StatusCode::BAD_REQUEST);
        }
    };
    let first_phone = request
        .phones
        .first()
        .map(|p| p.as_str())
        .unwrap_or("no number");
    Ok(format!(
        "{} is {} years old, {}'s first phone number is `{}`\n",
        request.name, request.age, request.name, first_phone
    ))
}

#[derive(Deserialize, Debug)]
struct Login {
    username: String,
    password: String,
}

fn process_login(info: Login) -> Result<String, StatusCode> {
    if info.username == "admin" && info.password == "password" {
        Ok("Login Success!".to_string())
    } else {
        Err(StatusCode::IM_A_TEAPOT)
    }
}

async fn get_with_query(Query(info): Query<Login>) -> Result<String, StatusCode> {
    process_login(info)
}

async fn post_with_form(Form(info): Form<Login>) -> Result<String, StatusCode> {
    process_login(info)
}

async fn test_body_and_err(
    _: &mut ServerContext,
    _: Request<FastStr>,
) -> Result<Response, StatusCode> {
    Ok(Response::default())
}

async fn map_body_and_err_inner(
    cx: &mut ServerContext,
    req: Request<Bytes>,
    next: Next<FastStr, StatusCode>,
) -> Response {
    let (parts, body) = req.into_parts();
    let body = FastStr::from_bytes(body).unwrap_or_default();
    let req = Request::from_parts(parts, body);
    next.run(cx, req).await.into_response()
}

async fn map_body_and_err_outer(
    cx: &mut ServerContext,
    req: Request,
    next: Next<Bytes>,
) -> Response {
    let (parts, body) = req.into_parts();
    let body = body.into_bytes().await.unwrap();
    let req = Request::from_parts(parts, body);
    next.run(cx, req).await.unwrap()
}

async fn get_and_post(
    u: Uri,
    m: Method,
    data: MaybeInvalid<FastStr>,
) -> Result<String, (StatusCode, &'static str)> {
    let msg = unsafe { data.assume_valid() };
    match m {
        Method::GET => Err((StatusCode::BAD_REQUEST, "Try POST something\n")),
        Method::POST => Ok(format!("{m} {u}\n\n{msg}\n")),
        _ => unreachable!(),
    }
}

async fn timeout_test() {
    tokio::time::sleep(Duration::from_secs(10)).await
}

async fn echo(PathParams(echo): PathParams<String>) -> String {
    echo
}

async fn add(PathParams((p1, p2)): PathParams<(usize, usize)>) -> String {
    format!("{}", p1 + p2)
}

async fn stream_test() -> Body {
    // build a `Vec<u8>` by a string
    let resp = "Hello, this is a stream.\n".as_bytes().iter().copied();
    // convert each byte to a `Bytes`
    let stream = stream! {
        for ch in resp.into_iter() {
            yield Bytes::from(vec![ch]);
        }
    };
    // map `Stream<Item = Bytes>` to `Steram<Item = Result<Frame<Bytes>, BoxError>>`
    Body::from_stream(stream.map(|b| Ok(Frame::data(b))))
}

async fn box_body_test() -> Body {
    let body = stream_test().await;
    Body::from_body(body)
}

async fn full_uri(uri: FullUri) -> String {
    format!("{uri}\n")
}

async fn redirect_to_index() -> Redirect {
    Redirect::permanent_redirect("/")
}

async fn trigger_panic() {
    panic!("PANIC!")
}

async fn sse() -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = stream! {
        loop {
            yield Ok(Event::new().event("ping"));
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    };
    Sse::new(stream)
}

struct State {
    foo: String,
    bar: usize,
}

async fn extension(Extension(state): Extension<Arc<State>>) -> String {
    format!("State {{ foo: {}, bar: {} }}\n", state.foo, state.bar)
}

async fn service_fn_test(cx: &mut ServerContext, req: Request) -> Result<String, Infallible> {
    Ok(format!("cx: {cx:?}, req: {req:?}"))
}

fn index_router() -> Router {
    // curl http://127.0.0.1:8080/
    Router::new().route("/", get(hello))
}

fn user_json_router() -> Router {
    Router::new()
        // curl http://localhost:8080/user/json_get
        .route("/user/json_get", get(json_get))
        // curl http://localhost:8080/user/json_post \
        //     -X POST \
        //     -H "Content-Type: application/json" \
        //     -d '{"name":"Foo", "age": 25, "phones":["Bar", "114514"]}'
        .route("/user/json_post", post(json_post))
        // curl http://localhost:8080/user/json_post_with_check \
        //     -X POST \
        //     -H "Content-Type: application/json" \
        //     -d '{"name":"Foo", "age": -1, "phones":["Bar", "114514"]}'
        //
        // Note that this is an invalid json
        .route("/user/json_post_with_check", post(json_post_with_check))
}

fn user_form_router() -> Router {
    Router::new().route(
        "/user/login",
        // curl "http://localhost:8080/user/login?username=admin&password=admin"
        // curl "http://localhost:8080/user/login?username=admin&password=password"
        get(get_with_query)
            // curl http://localhost:8080/user/login \
            //     -X POST \
            //     -d 'username=admin&password=admin'
            // curl http://localhost:8080/user/login \
            //     -X POST \
            //     -d 'username=admin&password=password'
            .post(post_with_form),
    )
}

fn body_error_router() -> Router {
    Router::new()
        .route(
            "/body_err/test",
            get_service(service_fn(test_body_and_err)).post_service(service_fn(test_body_and_err)),
        )
        .layer(middleware::from_fn(map_body_and_err_inner))
        .layer(middleware::from_fn(map_body_and_err_outer))
}

fn test_router() -> Router {
    Router::new()
        // curl http://127.0.0.1:8080/test/extract
        // curl http://127.0.0.1:8080/test/extract -X POST -d "114514"
        .route("/test/extract", get(get_and_post).post(get_and_post))
        // curl http://127.0.0.1:8080/test/timeout
        .route(
            "/test/timeout",
            get(timeout_test).layer(TimeoutLayer::new(
                Duration::from_secs(1),
                |_: &ServerContext| StatusCode::INTERNAL_SERVER_ERROR,
            )),
        )
        // curl -v http://127.0.0.1:8080/test/param/114514
        .route("/test/param/{:echo}", get(echo))
        // curl -v http://127.0.0.1:8080/test/param/114/514
        // curl -v http://127.0.0.1:8080/test/param/114/51A (400 Bad Request)
        .route("/test/param/{:p1}/{:p2}", get(add))
        // curl http://127.0.0.1:8080/test/extension
        .route("/test/extension", get(extension))
        // curl http://127.0.0.1:8080/test/service_fn
        .route("/test/service_fn", get_service(service_fn(service_fn_test)))
        // curl -v http://127.0.0.1:8080/test/stream
        .route("/test/stream", get(stream_test))
        // curl -v http://127.0.0.1:8080/test/body
        .route("/test/body", get(box_body_test))
        // curl -v http://127.0.0.1:8080/test/full_uri
        .route("/test/full_uri", get(full_uri))
        // curl -v http://127.0.0.1:8080/test/redirect
        // curl -L http://127.0.0.1:8080/test/redirect
        .route("/test/redirect", get(redirect_to_index))
        // curl -v http://127.0.0.1:8080/test/panic_500
        .route(
            "/test/panic_500",
            get(trigger_panic).layer(catch_panic::Layer::new(always_internal_error)),
        )
        // curl -v http://127.0.0.1:8080/test/panic_403
        .route(
            "/test/panic_403",
            get(trigger_panic).layer(catch_panic::Layer::new(fixed_payload(
                StatusCode::FORBIDDEN,
            ))),
        )
        // curl -L http://127.0.0.1:8080/test/redirect
        .route("/test/sse", get(sse))
        // curl -v http://127.0.0.1:8080/test/anyaddr?reject_me
        .layer(FilterLayer::new(|uri: Uri| async move {
            if uri.query().is_some() && uri.query().unwrap() == "reject_me" {
                Err(StatusCode::INTERNAL_SERVER_ERROR)
            } else {
                Ok(())
            }
        }))
}

fn docs_router() -> Router {
    Router::new().nest_service("/docs/", ServeDir::new("target/doc"))
}

// You can use the following commands for testing cookies
//
// ```bash
// # create a cookie jar for `curl`
// TMPFILE=$(mktemp --tmpdir cookie_jar.XXXXXX)
//
// # access it for more than one times!
// curl -v http://127.0.0.1:8080/ -b $TMPFILE -c $TMPFILE
// curl -v http://127.0.0.1:8080/ -b $TMPFILE -c $TMPFILE
// # ......
// ```
async fn tracing_from_fn(
    uri: Uri,
    peer: Address,
    cookie_jar: CookieJar,
    cx: &mut ServerContext,
    req: Request,
    next: Next,
) -> Response {
    tracing::info!("{:?}", *cookie_jar);
    let count = cookie_jar.get("count").map_or(0usize, |val| {
        val.value().to_string().parse().unwrap_or(0usize)
    });
    let start = std::time::Instant::now();
    let resp = next.run(cx, req).await;
    let elapsed = start.elapsed();

    tracing::info!("seq: {count}: {peer} request {uri}, cost {elapsed:?}");

    (
        (
            header::SET_COOKIE,
            cookie::Cookie::build(("count", format!("{}", count + 1)))
                .path("/")
                .max_age(cookie::Duration::days(1))
                .build()
                .to_string(),
        ),
        resp,
    )
        .into_response()
}

async fn headers_map_response(response: Response) -> impl IntoResponse {
    (
        [
            ("Access-Control-Allow-Origin", "*"),
            ("Access-Control-Allow-Headers", "*"),
            ("Access-Control-Allow-Method", "*"),
        ],
        response,
    )
}

fn timeout_handler(_: &ServerContext) -> StatusCode {
    tracing::error!("timeout");
    StatusCode::REQUEST_TIMEOUT
}

#[volo::main]
async fn main() {
    let subscriber = tracing_subscriber::FmtSubscriber::builder()
        .with_max_level(tracing::Level::TRACE)
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("setting default subscriber failed");

    let app = Router::new()
        .merge(index_router())
        .merge(user_json_router())
        .merge(user_form_router())
        .merge(body_error_router())
        .merge(test_router())
        .merge(docs_router())
        .layer(Extension(Arc::new(State {
            foo: "Foo".to_string(),
            bar: 114514,
        })))
        .layer(middleware::from_fn(tracing_from_fn))
        .layer(middleware::map_response(headers_map_response))
        .layer(TimeoutLayer::new(Duration::from_secs(5), timeout_handler));

    let addr: SocketAddr = "[::]:8080".parse().unwrap();
    let addr = volo::net::Address::from(addr);

    println!("Listening on {addr}");

    Server::new(app).run(addr).await.unwrap();
}
