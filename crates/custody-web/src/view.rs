use crate::model::{Asset, DepositAddress, Entry, Operations, PAGE};
use alloy_primitives::Address;
use maud::{html, Markup, DOCTYPE};

pub fn net(n: &str) -> &'static str {
    if n == "bsc-testnet" {
        "BSC Testnet"
    } else {
        "Mango Testnet"
    }
}
fn name(u: &str) -> &'static str {
    if u == "bob" {
        "Bob"
    } else {
        "Alice"
    }
}
fn address(n: &str, b: &[u8]) -> String {
    if n == "bsc-testnet" && b.len() == 20 {
        Address::from_slice(b).to_checksum(None)
    } else {
        format!("0x{}", hex::encode(b))
    }
}
fn digest(n: &str, b: &[u8]) -> String {
    if n == "mango-testnet" {
        bs58::encode(b).into_string()
    } else {
        format!("0x{}", hex::encode(b))
    }
}
fn units(raw: &str, decimals: i16) -> String {
    let digits = raw.trim_start_matches('-');
    let p = usize::try_from(decimals).unwrap_or(0);
    let padded = format!("{digits:0>width$}", width = p + 1);
    let (whole, fraction) = padded.split_at(padded.len() - p);
    let fraction = fraction.trim_end_matches('0');
    format!(
        "{}{}{}{}",
        if raw.starts_with('-') { "-" } else { "" },
        whole,
        if fraction.is_empty() { "" } else { "." },
        fraction
    )
}
fn network_logo(n: &str) -> Markup {
    html! {img.logo src=(if n=="bsc-testnet"{"/static/bsc.png"}else{"/static/mango.png"}) alt="";}
}
pub fn layout(title: &str, user: &str, area: &str, body: Markup) -> Markup {
    html! {(DOCTYPE) html lang="en" { head {meta charset="utf-8"; meta name="viewport" content="width=device-width,initial-scale=1";
        title {(title) " | Custody Desk"} link rel="stylesheet" href="/static/app.css"; }
    body {header {div.mast {div.brand {span.brandmark aria-hidden="true" {"C"} "Custody Desk"} span.testnet{"Testnet only"}}}
    main {div.tools {nav aria-label="Sections" {
        a.selected[area=="wallet"] href=(format!("/wallet/{user}")) {"Wallet"}
        a.selected[area=="activity"] href=(format!("/activity/{user}")) {"Activity"}
        a.selected[area=="ops"] href=(format!("/operations/{user}")) {"Operations"}
    } nav.switch aria-label="Demo account" {
        @for account in ["alice","bob"] { a.current[user==account] href=(format!("/{}/{account}",if area=="ops"{"operations"}else{area})) {(name(account))} }
    }} (body) } }}}
}
pub fn problem(title: &str, detail: &str, user: &str) -> Markup {
    layout(
        title,
        user,
        "wallet",
        html! {section {h1{(title)} p{(detail)} a.action href=(format!("/wallet/{user}")){"Back to wallet"}}},
    )
}
pub fn wallet(
    user: &str,
    assets: &[Asset],
    accounts: &[DepositAddress],
    notice: Option<&str>,
) -> Markup {
    let prefix = format!("/wallet/{user}");
    layout(
        "Wallet",
        user,
        "wallet",
        html! {
            div.heading {div {div.eyebrow {"ACCOUNT / " (name(user))} h1{"Wallet"}} a.secondary href=(format!("/activity/{user}")){"View activity"}}
            @if let Some(text)=notice {p.notice role="status" {(text)}}
            section {h2{"Asset balances"} div.assets {
            @for a in assets {article.asset {div.assethead {(network_logo(&a.net)) div {h3{(a.symbol)} small{(net(&a.net))}}}
                @if a.net=="mango-testnet" && !a.key.is_empty(){details {summary{"Coin type"} code style="overflow-wrap:anywhere"{(&a.key)}}}
                div.balance {(units(&a.available,a.decimals))} div.frozen {span{"Available"} span{"Frozen: " (units(&a.frozen,a.decimals))}}
            }} }}
            div.columns {section {h2{"Receive"} @for account in accounts {
                @let n=account.net.as_str();
                div.network {h3 {(network_logo(n)) (net(n))}
                @if !account.ready {p.notice{"Waiting for the " (net(n)) " scan cursor."}}
                @else if let Some(bytes)=&account.address {label {span.label{"Deposit address"} input.mono readonly value=(address(n,bytes)) aria-label=(format!("{} deposit address",net(n)));}}
                @else if account.requested {span.tag{"Awaiting signer"} " " a href=(&prefix){"Refresh"}}
                @else {form action=(format!("{prefix}/deposit")) method="post" {input type="hidden" name="network" value=(n);
                    button.secondary {"Create address"}}
                }
                @if n=="bsc-testnet" {p.note{"BNB deposits must be direct transfers. Internal BNB transfers are not credited."}}
                } }
            } section {h2{"Withdraw"} form method="post" action=(format!("{prefix}/withdraw")) {
                input type="hidden" name="request_id" value=(uuid::Uuid::new_v4());
                label {"Asset" select name="asset" required {
                    @for a in assets{option value=(if !a.key.is_empty(){&a.key}else if a.net=="mango-testnet"{"mango-native"}else{"native"}) {(a.symbol) " · " (net(&a.net)) @if a.net=="mango-testnet" && !a.key.is_empty(){" · " (&a.key)}}}
                }}
                label {"External address on selected network" input name="recipient" placeholder="0x…" required maxlength="66" minlength="42" autocomplete="off";}
                label {"Amount" input name="amount" placeholder="0.00" inputmode="decimal" required maxlength="512";}
                p.note{"Withdrawal fee: 0. Network gas is paid by the platform."}
                button.primary type="submit" {"Request withdrawal"}
            } }}
        },
    )
}
pub fn activity(
    user: &str,
    rows: &[Entry],
    deposits: bool,
    page: u32,
    notice: Option<&str>,
) -> Markup {
    let base = format!("/activity/{user}");
    let kind = if deposits { "deposits" } else { "withdrawals" };
    layout(
        "Activity",
        user,
        "activity",
        html! {
            div.heading {div {div.eyebrow {"ACCOUNT / " (name(user))} h1{"Activity"}}}
            @if let Some(text)=notice {p.notice role="status"{(text)}}
            div.filters {a.selected[deposits] href=(format!("{base}?kind=deposits")){"Deposits"}
                a.selected[!deposits] href=(format!("{base}?kind=withdrawals")){"Withdrawals"}}
            @if rows.is_empty() {p.empty{ "No " (kind) " on this page." }}
            @for e in rows.iter().take(PAGE){details.entry {summary {span.assethead {(network_logo(&e.net)) span {b{(e.symbol)} small{(net(&e.net))}}}
                strong{(units(&e.amount,e.decimals)) " " (e.symbol)} span.tag{(&e.status)}}
                dl {dt{"Reference"} dd{(&e.id)}
                    @if let Some(t)=&e.coin_type {dt{"Coin type"} dd{(t)}}
                    dt{(if deposits{"Sender"}else{"Recipient"})} dd{(e.address.as_ref().map(|a|address(&e.net,a)).unwrap_or_else(||"Not attributed".into()))}
                @if let Some(tx)=&e.tx {dt{(if deposits{"Transaction"}else{"Signed transaction"})} dd{(digest(&e.net,tx))}}
                @if let Some(tx)=&e.confirmed_tx {dt{"Finalized transaction"} dd{(digest(&e.net,tx))}}
                    @if let Some(h)=e.height {dt{"Finalized height"} dd{(h)}}
                    @if let Some(fee)=&e.gas {dt{"Platform net gas"} dd{@if e.net=="mango-testnet"{(units(fee,9)) " MGO"}@else{(units(fee,18)) " BNB"}}}
                    @if let Some(info)=&e.detail {dt{"Status detail"} dd{(info)}}
                }
            }}
            nav.pager aria-label="Pages" { @if page>1 {a href=(format!("{base}?kind={kind}&page={}",page-1)){"Previous"}}
                span {"Page " (page)} @if rows.len()>PAGE && page<u32::MAX {a href=(format!("{base}?kind={kind}&page={}",page+1)){"Next"}}}
        },
    )
}
pub fn operations(user: &str, ops: Operations) -> Markup {
    layout(
        "Operations",
        user,
        "ops",
        html! {
            div.heading {div{div.eyebrow{"PLATFORM"} h1{"Operations"}}}
            section {h2{"Payments"} div.statusline{span.tag{(if ops.paused{"Paused"}else{"Enabled"})}
                form action=(format!("/operations/{user}/payments")) method="post" {
                    input type="hidden" name="paused" value=(if ops.paused{"false"}else{"true"});
                    button.secondary{(if ops.paused{"Resume payments"}else{"Pause payments"})}
                }} p.note{"Pausing stops new signatures and broadcasts. Transactions already sent can still settle."}
            }
            section {h2{"Finalized cursors"}
                @for c in ops.cursors {div.record {b{(net(&c.net))}
                        span.tag{
                            @if c.issue.as_deref()==Some("finality") {
                                (if c.net=="bsc-testnet" {"Waiting for BSC finality"} else {"Waiting for a finalized Mango checkpoint"})
                            } @else if let Some(h)=c.next_height { "Scan interrupted at height " (h) }
                            @else if c.height.is_none() {"Initializing"}
                            @else if c.lag.is_some_and(|lag| lag>0) {
                                (if c.advancing {"Catching up"} else {"Behind sampled head"})
                            } @else if c.stale {"Awaiting fresh head sample"} @else {"At sampled head"}
                        }
                        @if let (Some(h),Some(d))=(c.height,c.digest.as_deref()){span{"Committed " (h)} code{(digest(&c.net,d))}}
                    }
                    @if let Some(at)=&c.issue_at {p.note{"Last failed check: " (at) " UTC"}}
                    @if let (Some(head),Some(lag),Some(at))=(c.head,c.lag,&c.measured) {
                        p.note { "Sampled head: " (head) " · Remaining to sample: " (lag) " · " (at) " UTC" @if c.stale{" · Stale sample"} }
                    } @else {p.note{"No head sample yet."}}
                }
            }
            section {h2{"Platform wallets"} @if ops.wallets.is_empty(){p.empty{"No platform wallets registered."}}
                @for w in ops.wallets {div.record{b{(net(&w.net))} span.tag{(w.role)} code{(address(&w.net,&w.address))}}}
            }
            section {h2{"Withdrawal queue"} @if ops.orders.is_empty(){p.empty{"No withdrawal orders."}}
                @for (n,s,count) in ops.orders {div.record {b{(net(&n))} span.tag{(s)} span{(count)}}}
            }
            section {h2{"Reconciliation"} @if ops.audits.is_empty(){p.empty{"No completed reconciliation runs."}}
                @for a in ops.audits {details.entry {summary{b{(net(&a.net)) " · " (a.symbol)} span{"Height " (a.height)} span.tag{(if a.matched{"Matched"}else{"Mismatch"})}}
                    dl{dt{"Book assets"} dd{(units(&a.book,a.decimals))} dt{"Observed on chain"} dd{(units(&a.observed,a.decimals))}
                        @if let Some(t)=a.coin_type {dt{"Coin type"} dd{(t)}}
                        dt{"Users: available / frozen"} dd{(units(&a.available,a.decimals)) " / " (units(&a.frozen,a.decimals))}
                        dt{"Platform capital"} dd{(units(&a.capital,a.decimals))} dt{"Net fees paid"} dd{(units(&a.fees,a.decimals))}
                    }
                }}
                @for w in ops.observed {details.entry{summary{b{(net(&w.net)) " · " (w.role)} span{(units(&w.observed,w.decimals)) " " (w.symbol)} span.tag{(if w.matched{"Matched"}else{"Mismatch"})}}
                    dl{dt{"Wallet"} dd{(address(&w.net,&w.address))} @if let Some(t)=w.coin_type {dt{"Coin type"} dd{(t)}} dt{"Expected"} dd{(units(&w.expected,w.decimals))} dt{"Observed"} dd{(units(&w.observed,w.decimals))}}
                }}
            }
            section {h2{"Recent sweeps"} @if ops.sweeps.is_empty(){p.empty{"No sweep transactions."}}
                @for s in ops.sweeps {details.entry {summary{span{(if s.kind=="gas_topup"{"Gas top-up"}else{"Sweep"})} strong{(units(&s.amount,s.decimals)) " " (s.symbol)} span.tag{(s.status)}}
                    dl{dt{"Network"} dd{(net(&s.net))} dt{"From"} dd{(address(&s.net,&s.sender))} dt{"To"} dd{(address(&s.net,&s.recipient))}
                        @if let Some(d)=s.detail{dt{"Status detail"} dd{(d)}}
                        @if let Some(f)=s.fee{dt{"Platform net gas"} dd{@if s.net=="mango-testnet"{(units(&f,9)) " MGO"}@else{(units(&f,18)) " BNB"}}}
                    }
                }}
            }
            section {h2{"Sweep checks"} @if ops.checks.is_empty(){p.empty{"No addresses checked."}}
                @for (n,a,d) in ops.checks {div.record{b{(net(&n))} span{(d)} code{(address(&n,&a))}}}
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_decimal_display() {
        for (raw, d, want) in [
            ("0", 9, "0"),
            ("1", 18, "0.000000000000000001"),
            ("123400", 4, "12.34"),
            ("-1", 0, "-1"),
        ] {
            assert_eq!(units(raw, d), want);
        }
    }
}
