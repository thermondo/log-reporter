use anyhow::Result;
use serde_json::json;

const LIRATO_USERNAME: &str = "heroku+76cc8300-be35-4c6c-a474-85836ca8c57e@solarwinds.com";
const LIBRATO_TOKEN: &str = "8b41266ad8e23a240f7fef2359c88dac7f52a917d35148c994d32edfd8fc75a4";

/// uses old API http://api-docs-archive.librato.com/
pub(crate) async fn test() -> Result<()> {
    let response = reqwest::Client::new()
        .post("https://metrics-api.librato.com/v1/metrics")
        .basic_auth(LIRATO_USERNAME, Some(LIBRATO_TOKEN))
        .json(&json!({
           "measure_time": 1481637660,
           "source": "my.app",
           "gauges": [
             {
               "name": "cpu",
               "value": 75,
               "source": "my.machine"
             }
           ],
           "counters": [
             {
               "name": "requests",
               "value": 1,
               "source": "my.machine"
             }
           ]
        }))
        .send()
        .await?;

    dbg!(&response.status());
    dbg!(response.text().await?);

    Ok(())
}

// curl \
//   -u $LIBRATO_USERNAME:$LIBRATO_TOKEN \
//   -H "Content-Type: application/json" \
//   -d '{
//     "tags": {
//       "region": "us-west",
//       "name": "web-prod-3"
//     },
//     "measurements": [
//       {
//         "name": "cpu",
//         "value": 4.5
//       },
//       {
//         "name": "memory",
//         "value": 10.5
//       }
//     ]
//   }' \
// -X POST \
// https://metrics-api.librato.com/v1/measurements
