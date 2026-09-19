/// Replaces leaf pane ids in a tmux layout with newly allocated ids and fixes
/// the checksum. Tmux layout leaves look like `80x24,0,0,12`.
pub fn remap(layout: &str, new_ids: &[u64]) -> Option<String> {
    let (_, body) = layout.split_once(',')?;
    remap_all(body, new_ids)
}

fn remap_all(body: &str, ids: &[u64]) -> Option<String> {
    let mut out = String::with_capacity(body.len());
    let b = body.as_bytes();
    let mut i = 0;
    let mut id_index = 0;
    while i < b.len() {
        let start = i;
        if !b[i].is_ascii_digit() {
            out.push(b[i] as char);
            i += 1;
            continue;
        }
        let read_num = |mut n: usize| {
            while n < b.len() && b[n].is_ascii_digit() {
                n += 1;
            }
            n
        };
        i = read_num(i);
        if i >= b.len() || b[i] != b'x' {
            out.push_str(&body[start..i]);
            continue;
        }
        i += 1;
        i = read_num(i);
        let mut valid = true;
        for _ in 0..3 {
            if i >= b.len() || b[i] != b',' {
                valid = false;
                break;
            }
            i += 1;
            let before = i;
            i = read_num(i);
            if before == i {
                valid = false;
                break;
            }
        }
        if valid && (i == b.len() || matches!(b[i], b',' | b'}' | b']')) {
            let last_comma = body[start..i].rfind(',')? + start;
            out.push_str(&body[start..=last_comma]);
            out.push_str(&ids.get(id_index)?.to_string());
            id_index += 1;
        } else {
            out.push_str(&body[start..i]);
        }
    }
    (id_index == ids.len()).then(|| format!("{:04x},{}", checksum(&out), out))
}

fn checksum(layout: &str) -> u16 {
    layout.bytes().fold(0u16, |sum, byte| {
        sum.rotate_right(1).wrapping_add(byte as u16)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn remaps_single_pane() {
        let got = remap("b25d,80x24,0,0,1", &[42]).unwrap();
        assert!(got.ends_with(",80x24,0,0,42"));
    }
    #[test]
    fn remaps_split_layout() {
        let got = remap("xxxx,160x48,0,0{80x48,0,0,1,79x48,81,0,2}", &[9, 10]).unwrap();
        assert!(got.ends_with("160x48,0,0{80x48,0,0,9,79x48,81,0,10}"));
    }
}
