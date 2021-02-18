use core::time::Duration;
use redis::RedisResult;

use crate::curiefense::config::limit::Limit;
use crate::curiefense::interface::{Decision, Tags};
use crate::curiefense::utils::{check_selector_cond, select_string, RequestInfo};

/// creates a connection to a redis server
fn redis_conn() -> anyhow::Result<redis::Connection> {
    let server = std::env::var("REDIS_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    println!("SERVER={}", server);
    let client = redis::Client::open(format!("redis://{}:6379/", server))?;
    let max_timeout = Duration::from_millis(100);
    let cnx = client.get_connection_with_timeout(max_timeout)?;
    cnx.set_read_timeout(Some(max_timeout))?;
    cnx.set_write_timeout(Some(max_timeout))?;
    Ok(cnx)
}

fn build_key(reqinfo: &RequestInfo, limit: &Limit) -> Option<String> {
    let kvals: Option<Vec<String>> = limit
        .key
        .iter()
        .map(|r| select_string(reqinfo, r))
        .collect();
    Some(format!("{:X}", md5::compute(kvals?.concat())))
}

fn get_ban_key(key: &str) -> String {
    format!("{:X}", md5::compute(format!("limit-ban-hash{}", key)))
}

fn is_banned(cnx: &mut redis::Connection, key: &str) -> bool {
    let ban_key = get_ban_key(&key);
    let q: redis::RedisResult<Option<u32>> = redis::cmd("GET").arg(&ban_key).query(cnx);
    q.unwrap_or(None).is_some()
}

fn limit_react(cnx: &mut redis::Connection, limit: &Limit, key: String) -> Decision {
    if limit.action.ban {
        let ban_key = get_ban_key(&key);
        if let Err(rr) = redis::cmd("SET").arg(ban_key).arg(1).query::<()>(cnx) {
            println!("*** Redis error {}", rr);
        }
    }
    Decision::Action(limit.action.clone())
}

fn redis_check_limit(
    cnx: &mut redis::Connection,
    key: &str,
    limit: u64,
    ttl: u64,
    pairvalue: Option<String>,
) -> RedisResult<bool> {
    let (mcurrent, mexpire): (Option<i64>, Option<i64>) = match &pairvalue {
        None => redis::pipe()
            .cmd("INCR")
            .arg(key)
            .cmd("TTL")
            .arg(key)
            .query(cnx)?,
        Some(pv) => redis::pipe()
            .cmd("SADD")
            .arg(key)
            .arg(pv)
            .ignore()
            .cmd("SCARD")
            .arg(key)
            .cmd("TTL")
            .arg(key)
            .query(cnx)?,
    };
    let current = mcurrent.unwrap_or(0);
    let expire = mexpire.unwrap_or(-1);

    println!("key={} limit={} ttl={} current={} expire={}", key, limit, ttl, current, expire);

    if expire < 0 {
        let _: () = redis::cmd("EXPIRE").arg(key).arg(ttl).query(cnx)?;
    }
    Ok(current > limit as i64)
}

pub fn limit_check(reqinfo: &RequestInfo, limits: &[Limit], tags: &mut Tags) -> Decision {
    // early return to avoid redis connection
    if limits.is_empty() {
        return Decision::Pass;
    }

    // we connect once for each request
    let mut redis = match redis_conn() {
        Ok(c) => c,
        Err(rr) => {
            println!("Could not connect to the redis server {}", rr);
            return Decision::Pass;
        }
    };

    for limit in limits {
        println!("Checking {:?}", limit);

        if limit
            .exclude
            .iter()
            .any(|selcond| check_selector_cond(reqinfo, tags, selcond))
        {
            continue;
        }
        if !limit
            .include
            .iter()
            .all(|selcond| check_selector_cond(reqinfo, tags, selcond))
        {
            continue;
        }

        // every matching ratelimit rule is tagged by name
        tags.insert(&limit.name);

        let key = match build_key(reqinfo, limit) {
            None => return Decision::Pass,
            Some(k) => k,
        };

        if limit.limit == 0 {
            return limit_react(&mut redis, limit, key);
        }

        if is_banned(&mut redis, &key) {
            return limit_react(&mut redis, limit, key);
        }

        let pairvalue = limit
            .pairwith
            .as_ref()
            .and_then(|sel| select_string(reqinfo, sel));

        match redis_check_limit(&mut redis, &key, limit.limit, limit.ttl, pairvalue) {
            Err(rr) => println!("*** Redis problem: {}", rr),
            Ok(true) => return limit_react(&mut redis, limit, key),
            Ok(false) => (),
        }
    }
    Decision::Pass
}
