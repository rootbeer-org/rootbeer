use data_encoding::HEXLOWER;
use std::sync::atomic::{AtomicBool, Ordering};

const SERIAL: Option<u64> = match option_env!("RB_SERIAL") {
    Some(serial) => match u64::from_str_radix(serial, 10) {
        Ok(serial) => Some(serial),
        Err(_) => panic!("RB_SERIAL must be a decimal serial"),
    },
    None => None,
};

const KEYS: [&str; 2] = [
    "d975f4927afcbf67bd3d6c27cd8e2e56b9b18dd4f2c694b180e1fd72f1833d9e",
    "cf374e572e4b224d834da6b8ec58657e3b1d96b94192e74de72b58c61da6b15b",
];

static IS_ALLOWED: AtomicBool = AtomicBool::new(false);
static IS_TRIED: AtomicBool = AtomicBool::new(false);

pub fn allow() {
    IS_ALLOWED.store(true, Ordering::Relaxed);
}

pub fn relaunch(reason: &str) {
    let Some(serial) = SERIAL else {
        return;
    };

    if !IS_ALLOWED.load(Ordering::Relaxed) || IS_TRIED.swap(true, Ordering::Relaxed) {
        return;
    }

    let keys = match keys() {
        Ok(keys) => keys,
        Err(error) => return eprintln!("warning: {error}"),
    };

    let release = match rootbeer_bootstrap::newer(&keys, serial) {
        Ok(Some(release)) => release,
        Ok(None) => return eprintln!("warning: no newer rb is released yet"),
        Err(error) => return eprintln!("warning: {error}"),
    };

    eprintln!("{reason}, so running rb {} instead", release.version());
    let rb = match rootbeer_bootstrap::fetch(&release) {
        Ok(rb) => rb,
        Err(error) => return eprintln!("warning: {error}"),
    };

    let error = rootbeer_bootstrap::exec(&rb);
    eprintln!("warning: running {}: {error}", rb.display());
}

fn keys() -> Result<Vec<[u8; 32]>, String> {
    KEYS.iter()
        .map(|key| {
            HEXLOWER
                .decode(key.as_bytes())
                .ok()
                .and_then(|key| key.try_into().ok())
                .ok_or_else(|| format!("bootstrap key {key} isn't 32 bytes of hex"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_keys_decode() {
        assert_eq!(keys().unwrap().len(), 2);
    }
}
