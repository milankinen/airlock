//! `GET /` — plain-text liveness probe for `admin.airlock`.

const MESSAGE: &str = r"
       _      _            _
  __ _(_)_ __| | ___   ___| | __
 / _` | | '__| |/ _ \ / __| |/ /
| (_| | | |  | | (_) | (__|   <
 \__,_|_|_|  |_|\___/ \___|_|\_\

";

pub async fn handle() -> &'static str {
    &MESSAGE[1..]
}
