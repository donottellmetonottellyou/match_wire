use crate::{
    cli::{Cli, Region},
    json_data::*,
    lowercase_vec,
    not_your_private_keys::LOCATION_PRIVATE_KEY,
    parse_hostname,
};

use tracing::{error, instrument};

use std::{
    collections::HashSet,
    fs::File,
    io::{self, Write},
    net::{IpAddr, ToSocketAddrs},
    path::Path,
    sync::LazyLock,
};

const MASTER_LOCATION_URL: &str = "https://api.findip.net/";

const MASTER_URL: &str = "https://master.iw4.zip/";
const JSON_SERVER_ENDPOINT: &str = "instance";
const FAVORITES_LOC: &str = "players2";
const FAVORITES: &str = "favourites.json";

const DEFAULT_SERVER_CAP: usize = 100;
const LOCAL_HOST: &str = "localhost";

const GAME_ID: &str = "H2M";
const CODE_NA: &str = "NA";
const CODE_EU: &str = "EU";

static APAC_CONT_CODES: LazyLock<HashSet<&str>> = LazyLock::new(populate_apac_cont_codes);

fn populate_apac_cont_codes() -> HashSet<&'static str> {
    const APAC_CONT_CODES_ARR: [&str; 3] = ["AS", "OC", "AF"];
    HashSet::from(APAC_CONT_CODES_ARR)
}

fn serialize_json(into: &mut std::fs::File, from: String) -> io::Result<()> {
    const COMMA: char = ',';
    let ips = if from.ends_with(COMMA) {
        &from[..from.len() - COMMA.len_utf8()]
    } else {
        from.as_str()
    };
    write!(into, "[{ips}]")
}

#[instrument(name = "filter", skip_all)]
pub async fn build_favorites(curr_dir: &Path, args: Cli) -> io::Result<()> {
    let mut ip_collected = 0;
    let mut ips = String::new();
    let mut favorites_json = File::create(curr_dir.join(format!("{FAVORITES_LOC}/{FAVORITES}")))?;
    let limit = args.limit.unwrap_or(DEFAULT_SERVER_CAP);

    if limit >= DEFAULT_SERVER_CAP {
        println!("NOTE: Currently the in game server browser breaks when you add more than 100 servers to favorites")
    }

    let mut servers = filter_server_list(&args)
        .await
        .map_err(|err| io::Error::other(format!("{err:?}")))?;

    println!(
        "{} servers match the prameters in the current query",
        servers.len()
    );

    if servers.len() > limit {
        servers.sort_unstable_by_key(|server| server.clientnum);
    }

    for server in servers.iter().rev() {
        ips.push_str(&format!("\"{}:{}\",", server.ip, server.port));
        ip_collected += 1;
        if ip_collected == limit {
            break;
        }
    }

    serialize_json(&mut favorites_json, ips)?;

    println!("{FAVORITES} updated with {ip_collected} entries");
    Ok(())
}

enum Task {
    Allowed(ServerInfo),
    Filtered,
    Error(io::Error),
}

#[instrument(level = "trace", skip_all)]
async fn filter_server_list(args: &Cli) -> reqwest::Result<Vec<ServerInfo>> {
    let instance_url = format!("{MASTER_URL}{JSON_SERVER_ENDPOINT}");
    let mut host_list = reqwest::get(instance_url.as_str())
        .await?
        .json::<Vec<HostData>>()
        .await?;

    let include = args.includes.as_ref().map(|s| lowercase_vec(s));
    let exclude = args.excludes.as_ref().map(|s| lowercase_vec(s));

    for i in (0..host_list.len()).rev() {
        for j in (0..host_list[i].servers.len()).rev() {
            if host_list[i].servers[j].game != GAME_ID {
                host_list[i].servers.swap_remove(j);
                continue;
            }

            if let Some(team_size_max) = args.team_size_max {
                if host_list[i].servers[j].maxclientnum > team_size_max * 2 {
                    host_list[i].servers.swap_remove(j);
                    continue;
                }
            }

            if let Some(player_min) = args.player_min {
                if host_list[i].servers[j].clientnum < player_min {
                    host_list[i].servers.swap_remove(j);
                    continue;
                }
            }

            let mut hostname_l = None;
            if let Some(ref strings) = include {
                hostname_l = Some(parse_hostname(&host_list[i].servers[j].hostname));
                if !strings
                    .iter()
                    .any(|string| hostname_l.as_ref().unwrap().contains(string))
                {
                    host_list[i].servers.swap_remove(j);
                    continue;
                }
            }
            if let Some(ref strings) = exclude {
                if hostname_l.is_none() {
                    hostname_l = Some(parse_hostname(&host_list[i].servers[j].hostname));
                }
                if strings
                    .iter()
                    .any(|string| hostname_l.as_ref().unwrap().contains(string))
                {
                    host_list[i].servers.swap_remove(j);
                }
            }
        }
        if host_list[i].servers.is_empty() {
            host_list.swap_remove(i);
        }
    }

    if let Some(region) = args.region {
        println!(
            "Determining region of {} servers...",
            host_list.iter().fold(0_usize, |mut count, host| {
                count += host.servers.len();
                count
            })
        );

        let client = reqwest::Client::new();

        let tasks = host_list.into_iter().fold(Vec::new(), |mut tasks, host| {
            host.servers.into_iter().for_each(|mut server| {
                let client = client.clone();
                if server.ip == LOCAL_HOST {
                    if let Ok(ip) = parse_possible_ipv6(&host.ip_address, &host.webfront_url) {
                        server.ip = ip.to_string()
                    };
                }
                tasks.push(tokio::spawn(async move {
                    let location = match try_location_lookup(&server, client).await {
                        Ok(loc) => loc,
                        Err(err) => return Task::Error(err),
                    };
                    match region {
                        Region::NA if location.code != CODE_NA => Task::Filtered,
                        Region::EU if location.code != CODE_EU => Task::Filtered,
                        Region::Apac if !APAC_CONT_CODES.contains(location.code.as_str()) => {
                            Task::Filtered
                        }
                        _ => Task::Allowed(server),
                    }
                }));
            });
            tasks
        });

        let mut failure_count = 0_usize;
        let mut server_list = Vec::new();

        for task in tasks {
            match task.await {
                Ok(result) => match result {
                    Task::Allowed(server) => server_list.push(server),
                    Task::Filtered => (),
                    Task::Error(err) => {
                        error!("{err}");
                        failure_count += 1
                    }
                },
                Err(err) => {
                    error!("{err:?}");
                    failure_count += 1
                }
            }
        }

        if failure_count > 0 {
            eprintln!("Failed to resolve location for {failure_count} server hoster(s)")
        }

        return Ok(server_list);
    }
    Ok(host_list.drain(..).flat_map(|host| host.servers).collect())
}

fn parse_possible_ipv6(ip: &str, webfront_url: &str) -> io::Result<IpAddr> {
    match resolve_address(ip) {
        Ok(ip) => Ok(ip),
        Err(err) => {
            const HTTP_ENDING: &str = "//";
            if let Some(i) = webfront_url.find(HTTP_ENDING) {
                const PORT_SEPERATOR: char = ':';
                let ip_start = i + HTTP_ENDING.len();
                let ipv6_slice = if let Some(j) = webfront_url[ip_start..].rfind(PORT_SEPERATOR) {
                    let ip_end = j + ip_start;
                    if ip_end <= ip_start {
                        return Err(io::Error::other("Failed to parse ip"));
                    }
                    &webfront_url[ip_start..ip_end]
                } else {
                    &webfront_url[ip_start..]
                };
                return resolve_address(ipv6_slice);
            }
            Err(err)
        }
    }
}

#[instrument(level = "trace", skip_all)]
async fn try_location_lookup(
    server: &ServerInfo,
    client: reqwest::Client,
) -> io::Result<Continent> {
    let format_url =
        |ip: IpAddr| -> String { format!("{MASTER_LOCATION_URL}{ip}{LOCATION_PRIVATE_KEY}") };
    let location_api_url = resolve_address(&server.ip).map(format_url)?;

    let api_response = client
        .get(location_api_url.as_str())
        .send()
        .await
        .map_err(|err| {
            io::Error::other(format!(
                "{err:?}, outbound url: {location_api_url}, server id: {}",
                server.id
            ))
        })?;

    match api_response.json::<ServerLocation>().await {
        Ok(json) => {
            if let Some(code) = json.continent {
                return Ok(code);
            }
            Err(io::Error::other(
                json.message
                    .unwrap_or_else(|| String::from("unknown error")),
            ))
        }
        Err(err) => Err(io::Error::other(format!(
            "{err:?}, outbound url: {location_api_url}, server id: {}",
            server.id
        ))),
    }
}

fn resolve_address(input: &str) -> io::Result<IpAddr> {
    let ip_trim = input.trim_matches('/').trim_matches(':');
    if ip_trim.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Ip can not be empty",
        ));
    }
    if let Ok(ip) = ip_trim.parse::<IpAddr>() {
        if ip.is_unspecified() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("Addr: {ip}, is not valid"),
            ));
        }
        return Ok(ip);
    }

    (ip_trim, 80)
        .to_socket_addrs()?
        .next()
        .map(|socket| socket.ip())
        .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "Hostname could not be resolved"))
}
