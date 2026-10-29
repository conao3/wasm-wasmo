use crate::query::{form_value, parse_form};
use crate::route::Route;
use crate::wire::{Request, Response};
use crate::xml::escape;

const XMLNS: &str = "https://sts.amazonaws.com/doc/2011-06-15/";

pub fn handle(route: &Route, req: &Request) -> Response {
    let form = parse_form(&req.body);
    match form_value(&form, "Action") {
        Some("GetCallerIdentity") => get_caller_identity(route),
        Some(action) => error(
            400,
            "InvalidAction",
            &format!("Could not find operation {action} for version 2011-06-15"),
        ),
        None => error(400, "MissingAction", "Missing Action"),
    }
}

fn get_caller_identity(route: &Route) -> Response {
    let account = escape(&route.account);
    let body = format!(
        "<GetCallerIdentityResponse xmlns=\"{XMLNS}\"><GetCallerIdentityResult><UserId>{account}</UserId><Account>{account}</Account><Arn>arn:aws:iam::{account}:root</Arn></GetCallerIdentityResult></GetCallerIdentityResponse>"
    );
    Response::new(200, "text/xml", body.into_bytes())
}

fn error(status: u16, code: &str, message: &str) -> Response {
    let body = format!(
        "<ErrorResponse xmlns=\"{XMLNS}\"><Error><Type>Sender</Type><Code>{}</Code><Message>{}</Message></Error></ErrorResponse>",
        escape(code),
        escape(message)
    );
    Response::new(status, "text/xml", body.into_bytes())
}
