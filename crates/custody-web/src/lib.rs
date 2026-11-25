mod model;
mod view;

use alloy_primitives::Address;
use axum::{
    extract::{DefaultBodyLimit, Path, Query, Request, State},
    http::{header, HeaderValue, Method, StatusCode},
    middleware::{self, Next},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
    Form, Router,
};
use custody_core::{AssetId, Network};
use custody_store::{wallets, withdrawals, PgPool};
use serde::Deserialize;

pub fn app(db: PgPool) -> Router {
    Router::new()
        .route("/", get(|| async { Redirect::to("/wallet/alice") }))
        .route("/wallet/{user}", get(wallet))
        .route("/wallet/{user}/deposit", post(deposit))
        .route("/wallet/{user}/withdraw", post(withdraw))
        .route("/activity/{user}", get(history))
        .route("/operations/{user}", get(ops))
        .route("/operations/{user}/payments", post(payments))
        .route(
            "/static/app.css",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/css")],
                    include_str!("../static/app.css"),
                )
            }),
        )
        .route(
            "/static/bsc.png",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "image/png")],
                    include_bytes!("../static/bsc.png").as_slice(),
                )
            }),
        )
        .route(
            "/static/mango.png",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "image/png")],
                    include_bytes!("../static/mango.png").as_slice(),
                )
            }),
        )
        .layer(DefaultBodyLimit::max(4096))
        .layer(middleware::from_fn(local_only))
        .with_state(db)
}

// No login by design: only serve loopback hostnames, disallow framing and
// reject browser writes without an exact same-origin Origin (no CORS).
async fn local_only(req: Request, next: Next) -> Response {
    let authority = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<axum::http::uri::Authority>().ok());
    let Some(host) = authority.filter(|a| {
        matches!(a.host(), "127.0.0.1" | "localhost" | "[::1]") && !a.as_str().contains('@')
    }) else {
        return (StatusCode::FORBIDDEN, "Loopback access only").into_response();
    };
    if req.method() == Method::POST
        && req
            .headers()
            .get(header::ORIGIN)
            .and_then(|v| v.to_str().ok())
            != Some(format!("http://{host}").as_str())
    {
        return (StatusCode::FORBIDDEN, "Local form origin required").into_response();
    }
    let mut response = next.run(req).await;
    let h = response.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static("default-src 'none'; style-src 'self'; img-src 'self'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'"));
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
}
type WebResult = Result<Response, Failure>;
struct Failure(StatusCode, &'static str, String);
impl Failure {
    fn input(user: &str, message: &'static str) -> Self {
        Self(StatusCode::UNPROCESSABLE_ENTITY, message, user.to_owned())
    }
    fn database(e: impl std::fmt::Display) -> Self {
        eprintln!("custody-web: {e}");
        Self(
            StatusCode::SERVICE_UNAVAILABLE,
            "Database operation unavailable. Check the local service log.",
            "alice".into(),
        )
    }
}
impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        (
            self.0,
            Html(view::problem("Request not completed", self.1, &self.2).into_string()),
        )
            .into_response()
    }
}
fn known(user: &str) -> Result<(), Failure> {
    if matches!(user, "alice" | "bob") {
        Ok(())
    } else {
        Err(Failure(
            StatusCode::NOT_FOUND,
            "Unknown demo account.",
            "alice".into(),
        ))
    }
}
#[derive(Default, Deserialize)]
struct Filters {
    kind: Option<String>,
    page: Option<u32>,
    notice: Option<String>,
}
async fn wallet(
    State(db): State<PgPool>,
    Path(user): Path<String>,
    Query(q): Query<Filters>,
) -> WebResult {
    known(&user)?;
    let (assets, addresses) = tokio::try_join!(
        async { model::assets(&db, &user).await.map_err(Failure::database) },
        async {
            model::deposit_addresses(&db, &user)
                .await
                .map_err(Failure::database)
        },
    )?;
    let notice = (q.notice.as_deref() == Some("address")).then_some("Deposit address requested.");
    Ok(Html(view::wallet(&user, &assets, &addresses, notice).into_string()).into_response())
}
#[derive(Deserialize)]
struct DepositForm {
    network: String,
}
async fn deposit(
    State(db): State<PgPool>,
    Path(user): Path<String>,
    Form(f): Form<DepositForm>,
) -> WebResult {
    known(&user)?;
    let n = match f.network.as_str() {
        "bsc-testnet" => Network::BscTestnet,
        "mango-testnet" => Network::MangoTestnet,
        _ => return Err(Failure::input(&user, "Unknown network.")),
    };
    if !wallets::network_ready(&db, n)
        .await
        .map_err(Failure::database)?
    {
        return Err(Failure::input(
            &user,
            "The selected network's scan cursor must be initialized before requesting an address.",
        ));
    }
    wallets::request(&db, n, wallets::Purpose::Deposit(&user))
        .await
        .map_err(Failure::database)?;
    Ok(Redirect::to(&format!("/wallet/{user}?notice=address")).into_response())
}
#[derive(Deserialize)]
struct WithdrawForm {
    request_id: String,
    asset: String,
    amount: String,
    recipient: String,
}
fn parse_recipient(s: &str) -> Option<Address> {
    let hex = s.strip_prefix("0x")?;
    if hex.len() != 40 {
        return None;
    }
    let addr: Address = s.parse().ok()?;
    let mixed =
        hex.bytes().any(|b| b.is_ascii_lowercase()) && hex.bytes().any(|b| b.is_ascii_uppercase());
    (!addr.is_zero() && (!mixed || addr.to_checksum(None) == s)).then_some(addr)
}
async fn withdraw(
    State(db): State<PgPool>,
    Path(user): Path<String>,
    Form(f): Form<WithdrawForm>,
) -> WebResult {
    known(&user)?;
    let mango = f.asset == "mango-native" || f.asset.contains("::");
    let recipient = if mango {
        let mut bytes = [0u8; 32];
        let raw = f.recipient.trim().strip_prefix("0x").unwrap_or("");
        if hex::decode_to_slice(raw, &mut bytes).is_err() || bytes == [0; 32] {
            return Err(Failure::input(
                &user,
                "Enter a full 32-byte external Mango address.",
            ));
        }
        bytes.to_vec()
    } else {
        parse_recipient(f.recipient.trim())
            .ok_or_else(|| {
                Failure::input(
            &user,
            "Enter a valid external BSC address. Check the checksum of mixed-case addresses.",
        )
            })?
            .to_vec()
    };
    if uuid::Uuid::parse_str(&f.request_id).is_err() {
        return Err(Failure::input(
            &user,
            "Invalid form reference. Open a fresh withdrawal form.",
        ));
    }
    let allowed = model::assets(&db, &user).await.map_err(Failure::database)?;
    let key = if f.asset == "native" || f.asset == "mango-native" {
        ""
    } else {
        &f.asset
    };
    let network = if mango {
        Network::MangoTestnet
    } else {
        Network::BscTestnet
    };
    let a = allowed
        .iter()
        .find(|a| a.net == custody_store::network_key(network) && a.key == key)
        .ok_or_else(|| Failure::input(&user, "Unsupported withdrawal asset."))?;
    withdrawals::raw_amount(&f.amount, a.decimals as u8, network).map_err(|_| {
        Failure::input(
            &user,
            "Enter a positive decimal amount within this asset's precision and on-chain range.",
        )
    })?;
    let asset = if mango && !key.is_empty() {
        AssetId::MangoTestnetCoin(a.key.clone())
    } else if mango {
        AssetId::MangoTestnetMgo
    } else if key.is_empty() {
        AssetId::BscTestnetBnb
    } else {
        let mut contract = [0; 20];
        hex::decode_to_slice(key, &mut contract).map_err(Failure::database)?;
        AssetId::BscTestnetErc20(contract)
    };
    withdrawals::submit(
        &db,
        withdrawals::Request {
            user: &user,
            request_id: &f.request_id,
            asset,
            recipient: recipient.as_slice(),
            amount: &f.amount,
        },
    )
    .await
    .map_err(|e| match e.to_string().as_str() {
        "insufficient available balance" => {
            Failure::input(&user, "Insufficient available balance.")
        }
        "withdrawals must target an external address" => {
            Failure::input(&user, "Use an external address, not a managed wallet.")
        }
        "client request ID already used for a different withdrawal" => Failure::input(
            &user,
            "This form was already submitted with different details.",
        ),
        _ => Failure::database(e),
    })?;
    Ok(Redirect::to(&format!("/activity/{user}?kind=withdrawals&notice=request")).into_response())
}
async fn history(
    State(db): State<PgPool>,
    Path(user): Path<String>,
    Query(q): Query<Filters>,
) -> WebResult {
    known(&user)?;
    let deposits = match q.kind.as_deref() {
        None | Some("deposits") => true,
        Some("withdrawals") => false,
        _ => return Err(Failure::input(&user, "Unknown activity type.")),
    };
    let page = q.page.unwrap_or(1);
    if page == 0 {
        return Err(Failure::input(&user, "Pages start at 1."));
    }
    let rows = model::history(&db, &user, deposits, page)
        .await
        .map_err(Failure::database)?;
    let message = (q.notice.as_deref() == Some("request") && !deposits)
        .then_some("Withdrawal request recorded. Follow its status below.");
    Ok(Html(view::activity(&user, &rows, deposits, page, message).into_string()).into_response())
}
async fn ops(State(db): State<PgPool>, Path(user): Path<String>) -> WebResult {
    known(&user)?;
    Ok(Html(
        view::operations(
            &user,
            model::operations(&db).await.map_err(Failure::database)?,
        )
        .into_string(),
    )
    .into_response())
}
#[derive(Deserialize)]
struct Control {
    paused: bool,
}
async fn payments(
    State(db): State<PgPool>,
    Path(user): Path<String>,
    Form(f): Form<Control>,
) -> WebResult {
    known(&user)?;
    let mut lock = db.begin().await.map_err(Failure::database)?;
    sqlx::query("SELECT paused FROM payment_control WHERE singleton FOR UPDATE")
        .fetch_one(&mut *lock)
        .await
        .map_err(Failure::database)?;
    let updated=sqlx::query("UPDATE payment_control SET paused=$1 WHERE singleton AND ($1 OR NOT EXISTS(SELECT 1 FROM reconciliation_holds))")
        .bind(f.paused)
        .execute(&mut *lock)
        .await
        .map_err(Failure::database)?;
    if updated.rows_affected() == 0 {
        return Err(Failure::input(&user,"Resolve the outstanding reconciliation difference and run a clean audit before resuming payments."));
    }
    lock.commit().await.map_err(Failure::database)?;
    Ok(Redirect::to(&format!("/operations/{user}")).into_response())
}
