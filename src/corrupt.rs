pub fn corrupt(data: &[u8]) -> Vec<u8> {
    let mut result = Vec::with_capacity(data.len() + data.len() / 128);
    for i in 0..data.len() {
        if data[i] == 0x0A && (i == 0 || data[i - 1] != 0x0D) {
            result.push(0x0D);
        }
        result.push(data[i]);
    }
    result
}

pub fn find_standalone_lf(data: &[u8]) -> Vec<usize> {
    let mut positions = Vec::new();
    for i in 0..data.len() {
        if data[i] == 0x0A && (i == 0 || data[i - 1] != 0x0D) {
            positions.push(i);
        }
    }
    positions
}
