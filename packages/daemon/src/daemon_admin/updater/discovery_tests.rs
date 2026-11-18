use super::*;
use std::io::BufRead;

const REPOSITORY: &str = "example/paseo";
const DOWNLOAD: &str = "https://github.com/example/paseo/releases/download";
const API: &str = "https://api.github.com/repos/example/paseo";
const LIST: &str = "https://api.github.com/repos/example/paseo/releases?per_page=100&page=1";
const LATEST: &str =
    "https://github.com/example/paseo/releases/latest/download/components-stable.json";

#[derive(Default)]
struct GitHub {
    responses: HashMap<String, Metadata>,
    requests: Mutex<Vec<String>>,
}

impl MetadataSource for GitHub {
    async fn get_metadata(&self, url: &Url) -> Result<Metadata> {
        self.requests.lock().unwrap().push(url.to_string());
        Ok(match self.responses.get(url.as_str()) {
            Some(Metadata::Bytes(bytes)) => Metadata::Bytes(bytes.clone()),
            Some(Metadata::Redirect(url)) => Metadata::Redirect(url.clone()),
            Some(Metadata::NotFound) => Metadata::NotFound,
            None => bail!("Unexpected request: {url}"),
        })
    }
}

impl GitHub {
    fn json(&mut self, url: String, value: &Value) {
        self.responses
            .insert(url, Metadata::Bytes(serde_json::to_vec(value).unwrap()));
    }

    fn take_requests(&self) -> Vec<String> {
        std::mem::take(&mut *self.requests.lock().unwrap())
    }

    fn latest(&mut self, version: &str) {
        self.responses.insert(
            LATEST.into(),
            Metadata::Redirect(
                Url::parse(&format!("{DOWNLOAD}/v{version}/components-stable.json")).unwrap(),
            ),
        );
    }

    fn publish(
        &mut self,
        version: &str,
        target: &str,
        run: u64,
        reused: Option<&Value>,
        inventory_suffix: &[u8],
    ) -> (Value, Value) {
        let tag = format!("v{version}");
        let base = format!("{DOWNLOAD}/{tag}");
        let source = format!("{run:040x}");
        let tag_object = format!("{:040x}", run + 1000);
        let channel = if version.contains('-') {
            "beta"
        } else {
            "stable"
        };
        let name = format!("Paseo-Daemon-{version}-{target}.tar.gz");
        let sha256 = "c".repeat(64);
        let component = reused.cloned().unwrap_or_else(|| json!({
            "id":"daemon", "target":target,
            "identity":{"version":version,"sourceSha":source,"inputHash":"b".repeat(64),"abiVersion":1},
            "artifact":{"url":format!("{base}/{name}"),"originTag":tag,"format":"tar.gz","size":123,"sha256":sha256},
            "executable":"paseo","executableSha256":"d".repeat(64),"requires":{}
        }));
        let target_inventory =
            json!({"components":{"daemon":component},"packages":[],"atomicGroups":[]});
        let inventory = json!({"schemaVersion":1,"channel":channel,"version":version,
            "sourceSha":source,"targets":{target:target_inventory}});
        let mut bytes = serde_json::to_vec(&inventory).unwrap();
        bytes.extend_from_slice(inventory_suffix);
        let proof = json!({"schemaVersion":2,"workflow":".github/workflows/desktop-release.yml",
            "sha":source,"sourceTag":tag,"version":version,"channel":channel,"runId":run,
            "target":target,"components":target_inventory["components"],"packages":[],
            "files":[{"name":name,"size":123,"sha256":sha256}]});
        let proof = serde_json::to_vec(&proof).unwrap();
        let inventory_name = format!("components-{channel}.json");
        let provenance_name = format!("provenance-components-{channel}.json");
        let proof_name = format!("provenance-{target}.json");
        let provenance = json!({"schemaVersion":2,"workflow":".github/workflows/desktop-release.yml",
            "sourceSha":source,"version":version,"channel":channel,"runId":run,
            "inventory":{"name":inventory_name,"size":bytes.len(),"sha256":format!("{:x}", Sha256::digest(&bytes))},
            "targets":{target:format!("{:x}", Sha256::digest(&proof))}});
        let provenance = serde_json::to_vec(&provenance).unwrap();
        let asset = |name: &str, size: usize| {
            json!({"name":name,"size":size,
            "browser_download_url":format!("{base}/{name}"),"uploader":{"login":"github-actions[bot]"}})
        };
        let mut assets = vec![
            asset(&inventory_name, bytes.len()),
            asset(&provenance_name, provenance.len()),
        ];
        if reused.is_none() {
            assets.extend([asset(&proof_name, proof.len()), asset(&name, 123)]);
            self.responses
                .insert(format!("{base}/{proof_name}"), Metadata::Bytes(proof));
        }
        let published = json!({"tag_name":tag,"draft":false,"prerelease":channel == "beta",
            "target_commitish":source,"author":{"login":"github-actions[bot]"},"assets":assets});
        self.json(format!("{API}/releases/tags/{tag}"), &published);
        self.json(format!("{API}/actions/runs/{run}"), &json!({
            "path":".github/workflows/desktop-release.yml","event":"workflow_dispatch",
            "status":"completed","conclusion":"success","head_repository":{"full_name":REPOSITORY}
        }));
        self.json(
            format!("{API}/git/ref/tags/{tag}"),
            &json!({"object":{"type":"tag","sha":tag_object}}),
        );
        self.json(
            format!("{API}/git/tags/{tag_object}"),
            &json!({"object":{"type":"commit","sha":source}}),
        );
        self.responses
            .insert(format!("{base}/{inventory_name}"), Metadata::Bytes(bytes));
        self.responses.insert(
            format!("{base}/{provenance_name}"),
            Metadata::Bytes(provenance),
        );
        (published, component)
    }

    async fn check(&self, home: &Path, channel: &str) -> Result<Release> {
        discover(
            self,
            home,
            DiscoveryTarget {
                repository: REPOSITORY,
                channel,
                platform: "linux",
                arch: "x64",
            },
        )
        .await
    }
}

fn rest_calls(requests: &[String]) -> usize {
    requests.iter().filter(|url| url.starts_with(API)).count()
}

#[tokio::test]
async fn release_discovery_stable_cache_measures_rest_calls_and_reverifies_changed_bytes() {
    let home = tempfile::tempdir().unwrap();
    let mut github = GitHub::default();
    github.publish("2.0.0", "linux-x64", 42, None, b"");
    github.latest("2.0.0");
    let first = github.check(home.path(), "stable").await.unwrap();
    let cold = github.take_requests();
    // Fresh transport and the same home prove the second check uses disk, not memory.
    let mut github = GitHub {
        responses: github.responses,
        ..Default::default()
    };
    assert_eq!(github.check(home.path(), "stable").await.unwrap(), first);
    let warm = github.take_requests();
    eprintln!(
        "stable REST requests: first={}, second={}",
        rest_calls(&cold),
        rest_calls(&warm)
    );
    assert_eq!(rest_calls(&cold), 4, "{cold:?}");
    assert_eq!(rest_calls(&warm), 0, "{warm:?}");
    assert_eq!(
        warm,
        [
            LATEST.to_owned(),
            format!("{DOWNLOAD}/v2.0.0/components-stable.json")
        ]
    );

    github.publish("2.0.0", "linux-x64", 42, None, b"\n");
    let changed = github.check(home.path(), "stable").await.unwrap();
    assert_ne!(changed.identity(), first.identity());
    assert_eq!(rest_calls(&github.take_requests()), 4);
    assert_eq!(github.check(home.path(), "stable").await.unwrap(), changed);
    assert_eq!(rest_calls(&github.take_requests()), 0);

    fs::write(
        home.path()
            .join("native-update-cache/verified-releases-v1.json"),
        b"{",
    )
    .unwrap();
    assert_eq!(github.check(home.path(), "stable").await.unwrap(), changed);
    assert_eq!(rest_calls(&github.take_requests()), 4);
    assert_eq!(github.check(home.path(), "stable").await.unwrap(), changed);
    assert_eq!(rest_calls(&github.take_requests()), 0);
}

#[tokio::test]
async fn release_discovery_carries_forward_only_verified_origins() {
    let home = tempfile::tempdir().unwrap();
    let mut github = GitHub::default();
    let (_, origin) = github.publish("1.0.0", "linux-x64", 41, None, b"");
    github.publish("2.0.0", "linux-x64", 42, Some(&origin), b"");
    github.latest("2.0.0");
    let origin_commit = format!("{API}/git/tags/{:040x}", 1041);
    let valid = github.responses.remove(&origin_commit).unwrap();
    github.json(
        origin_commit.clone(),
        &json!({"object":{"type":"commit","sha":"f".repeat(40)}}),
    );
    let error = github.check(home.path(), "stable").await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("Release tag commit does not match")
    );
    assert!(cache::entries(home.path()).await.is_empty());
    assert_eq!(rest_calls(&github.take_requests()), 8);

    github.responses.insert(origin_commit, valid);
    assert_eq!(
        github.check(home.path(), "stable").await.unwrap().version,
        "1.0.0"
    );
    assert_eq!(rest_calls(&github.take_requests()), 8);
    assert_eq!(
        github.check(home.path(), "stable").await.unwrap().version,
        "1.0.0"
    );
    assert_eq!(rest_calls(&github.take_requests()), 0);
}

#[tokio::test]
async fn release_discovery_beta_and_missing_target_fall_back_to_list() {
    let home = tempfile::tempdir().unwrap();
    let mut github = GitHub::default();
    let (beta, _) = github.publish("3.0.0-beta.1", "linux-x64", 43, None, b"");
    github.json(LIST.into(), &json!([beta]));
    assert_eq!(
        github.check(home.path(), "beta").await.unwrap().version,
        "3.0.0-beta.1"
    );
    let beta_requests = github.take_requests();
    assert_eq!(beta_requests.first().unwrap(), LIST);
    assert!(!beta_requests.iter().any(|request| request == LATEST));

    let (newest, _) = github.publish("2.0.0", "linux-arm64", 42, None, b"");
    let (older, _) = github.publish("1.0.0", "linux-x64", 41, None, b"");
    github.latest("2.0.0");
    github.json(LIST.into(), &json!([newest, older]));
    assert_eq!(
        github.check(home.path(), "stable").await.unwrap().version,
        "1.0.0"
    );
    let fallback = github.take_requests();
    assert_eq!(fallback.first().unwrap(), LATEST);
    assert_eq!(
        fallback.iter().find(|url| url.starts_with(API)).unwrap(),
        LIST
    );
    assert_eq!(rest_calls(&fallback), 4);
    github.check(home.path(), "stable").await.unwrap();
    assert_eq!(rest_calls(&github.take_requests()), 1);

    github.responses.insert(LATEST.into(), Metadata::NotFound);
    github.check(home.path(), "stable").await.unwrap();
    assert_eq!(rest_calls(&github.take_requests()), 1);
}

#[tokio::test]
async fn release_discovery_rejects_inventory_that_disagrees_with_alias_tag() {
    let home = tempfile::tempdir().unwrap();
    let mut github = GitHub::default();
    github.publish("2.0.0", "linux-x64", 42, None, b"");
    github.latest("3.0.0");
    let bytes = github
        .responses
        .remove(&format!("{DOWNLOAD}/v2.0.0/components-stable.json"))
        .unwrap();
    github
        .responses
        .insert(format!("{DOWNLOAD}/v3.0.0/components-stable.json"), bytes);
    assert!(
        github
            .check(home.path(), "stable")
            .await
            .unwrap_err()
            .to_string()
            .contains("collection identity mismatch")
    );
    assert_eq!(rest_calls(&github.take_requests()), 0);
}

#[tokio::test]
async fn release_discovery_concurrent_callers_share_one_flight() {
    let home = tempfile::tempdir().unwrap();
    let mut github = GitHub::default();
    github.publish("2.0.0", "linux-x64", 42, None, b"");
    github.latest("2.0.0");
    let github = Arc::new(github);
    let checks = ReleaseChecks::default();
    let start = |home: PathBuf| {
        let github = Arc::clone(&github);
        async move {
            github
                .check(&home, "stable")
                .await
                .map(Arc::new)
                .map_err(|error| CheckError(Arc::new(error)))
        }
        .boxed()
    };
    let (first, second) = tokio::join!(
        checks.flight(home.path(), start),
        checks.flight(home.path(), start)
    );
    assert!(Arc::ptr_eq(&first.unwrap(), &second.unwrap()));
    assert_eq!(rest_calls(&github.take_requests()), 4);
    checks.flight(home.path(), start).await.unwrap();
    assert_eq!(
        github.take_requests().len(),
        2,
        "a new check downloads the inventory again"
    );
}

#[tokio::test]
async fn release_discovery_http_maps_github_rate_limit_with_reset_time() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
    let respond = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = std::io::BufReader::new(&stream);
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                break;
            }
        }
        write!(stream, "HTTP/1.1 403 Forbidden\r\nx-ratelimit-remaining: 0\r\nx-ratelimit-reset: 1795019820\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
    });
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let error = json_bytes(&client, url).await.unwrap_err();
    respond.join().unwrap();
    let typed = error.downcast_ref::<GitHubRateLimited>().unwrap();
    assert_eq!(typed.retry_at.timestamp(), 1795019820);

    let now = DateTime::from_timestamp(1795019820, 0).unwrap();
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("retry-after", "120".parse().unwrap());
    let retry =
        GitHubRateLimited::from_response(reqwest::StatusCode::TOO_MANY_REQUESTS, &headers, now)
            .unwrap();
    assert_eq!(retry.retry_at.timestamp(), 1795019940);
    assert!(
        GitHubRateLimited::from_response(
            reqwest::StatusCode::FORBIDDEN,
            &reqwest::header::HeaderMap::new(),
            now
        )
        .is_none()
    );
}
