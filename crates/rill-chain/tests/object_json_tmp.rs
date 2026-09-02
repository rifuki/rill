//! Does a masked GetObject expose the wallet's rule set without a simulation?
use sui_rpc::client::Client;
use sui_rpc::proto::sui::rpc::v2::GetObjectRequest;

const TESTNET: &str = "https://fullnode.testnet.sui.io:443";
const WALLET: &str = "0x20391fa91aec7a12b6657902af80036e125d1beff6621fe2eb73cfd032a04e5d";

async fn fetch(paths: &[&str]) {
    let client = Client::new(TESTNET).unwrap();
    let mut request = GetObjectRequest::default();
    request.object_id = Some(WALLET.to_owned());
    request.read_mask = Some(prost_types::FieldMask {
        paths: paths.iter().map(|p| (*p).to_owned()).collect(),
    });
    println!("\n===== mask {paths:?} =====");
    match client.clone().ledger_client().get_object(request).await {
        Err(s) => println!("ERR code={:?} msg={}", s.code(), s.message()),
        Ok(r) => {
            let o = r.into_inner().object.unwrap();
            println!("json is_some = {}", o.json.is_some());
            if let Some(j) = o.json.as_ref() {
                let s = format!("{j:?}");
                println!("json len = {}", s.len());
                println!("{s}");
            }
            println!("contents is_some = {}", o.contents.is_some());
            if let Some(c) = o.contents.as_ref() {
                let b = c.value.clone().unwrap_or_default();
                println!("contents len = {}", b.len());
                println!(
                    "contents hex = {}",
                    b.iter().map(|x| format!("{x:02x}")).collect::<String>()
                );
            }
        }
    }
}

#[tokio::test]
#[ignore]
async fn probe_masks() {
    fetch(&[
        "object_id",
        "version",
        "digest",
        "object_type",
        "owner",
        "json",
    ])
    .await;
    fetch(&["object_id", "contents"]).await;
    fetch(&["*"]).await;
}
