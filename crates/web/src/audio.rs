//! 合成好的音频暂存在内存里，页面凭 id 来取。
//!
//! **为什么不让音频直接进 `OverlayState`**：那是每帧都推给画面的状态，
//! 一段三秒的 MP3 有二十来 KB，塞进去等于每次推送都复制几十 KB，
//! 而且要 base64 才能过 JSON。更糟的是讲解词里含正确答案，
//! 一旦进了状态就等于**在收答期间把答案推给了画面**（防泄题见 director）。
//! 所以状态里只放一个 id，音频单独走 `GET /voice/{id}`。

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::Mutex;

/// 音频的取件码。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AudioId(String);

impl AudioId {
    /// 从路径段还原一个取件码。
    ///
    /// 不做格式校验：这不是安全边界，只是个查表的 key，形状不对自然查不到、
    /// 返回 `None`。真正的边界是这个服务只监听 127.0.0.1。
    pub fn new(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for AudioId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

struct Entry {
    bytes: Arc<[u8]>,
}

#[derive(Default)]
struct Inner {
    map: HashMap<AudioId, Entry>,
    /// 先进先出。用来淘汰最旧的，而不是随机砍。
    order: VecDeque<AudioId>,
    /// 当前占用的总字节数。
    bytes: usize,
    next: u64,
}

/// 内存音频表。
///
/// 按**字节数**而不是条数限容量：一条讲解可能有几十 KB，一条「请作答」
/// 只有几 KB，按条数限会让容量在两者之间乱跳。
pub struct AudioStore {
    inner: Mutex<Inner>,
    /// 上限，默认 64 MB——一场直播几百条语音，绰绰有余。
    capacity: usize,
}

impl Default for AudioStore {
    fn default() -> Self {
        Self::new(64 * 1024 * 1024)
    }
}

impl AudioStore {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            capacity,
        }
    }

    /// 存一段音频，返回取件码。
    pub fn put(&self, bytes: Vec<u8>) -> AudioId {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());

        inner.next += 1;
        let id = AudioId(format!("v{}", inner.next));
        let len = bytes.len();

        inner.order.push_back(id.clone());
        inner.map.insert(id.clone(), Entry { bytes: Arc::from(bytes) });
        inner.bytes += len;

        // 超了就从头淘汰。这里必须循环而不是 if：一次 put 可能就让容量
        // 越过上限很多（大段讲解），砍一条不够。
        while inner.bytes > self.capacity && inner.order.len() > 1 {
            let Some(old) = inner.order.pop_front() else {
                break;
            };
            if let Some(entry) = inner.map.remove(&old) {
                inner.bytes -= entry.bytes.len();
            }
        }

        id
    }

    /// 取一段音频。取不到返回 `None`（页面该安静地跳过，不要报错刷屏）。
    pub fn get(&self, id: &AudioId) -> Option<Arc<[u8]>> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.map.get(id).map(|e| Arc::clone(&e.bytes))
    }

    /// 丢掉一条。讲解在公布后才发，之前一直压在服务端——
    /// 万一这一轮被跳过，得能清掉，不然内存只涨不落。
    pub fn remove(&self, id: &AudioId) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = inner.map.remove(id) {
            inner.bytes -= entry.bytes.len();
            inner.order.retain(|x| x != id);
        }
    }

    /// 当前占用字节数。测试和排查用。
    pub fn bytes(&self) -> usize {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_then_get_roundtrips() {
        let store = AudioStore::new(1024);
        let id = store.put(vec![1, 2, 3]);
        assert_eq!(&*store.get(&id).unwrap(), &[1, 2, 3]);
    }

    #[test]
    fn missing_id_is_none_not_panic() {
        let store = AudioStore::new(1024);
        assert!(store.get(&AudioId("v999".to_owned())).is_none());
    }

    #[test]
    fn oldest_is_evicted_when_over_capacity() {
        let store = AudioStore::new(10);
        let a = store.put(vec![0; 6]);
        let b = store.put(vec![0; 6]);
        // 先来的该被挤掉了。
        assert!(store.get(&a).is_none(), "最旧的应该被淘汰");
        assert!(store.get(&b).is_some(), "新的必须还在");
        assert!(store.bytes() <= 10, "占用不该超过上限");
    }

    #[test]
    fn eviction_loops_until_under_capacity() {
        // 回归测试。淘汰写成 `if` 而不是 `while` 的话，一条超大音频
        // 进来只砍掉一条旧的，容量还是会超——而且超了之后再也降不下来，
        // 直播跑几小时内存就一直涨。
        let store = AudioStore::new(10);
        store.put(vec![0; 4]);
        store.put(vec![0; 4]);
        store.put(vec![0; 4]);
        // 现在进来一条 30 字节的，必须把前面几条全挤掉才可能不超。
        store.put(vec![0; 30]);
        assert!(store.bytes() <= 30, "超大条目进来后容量不该爆掉");
    }

    #[test]
    fn remove_frees_capacity() {
        let store = AudioStore::new(1024);
        let id = store.put(vec![0; 100]);
        assert_eq!(store.bytes(), 100);
        store.remove(&id);
        assert_eq!(store.bytes(), 0);
        assert!(store.get(&id).is_none());
    }

    #[test]
    fn ids_are_unique() {
        let store = AudioStore::new(1024);
        let a = store.put(vec![0; 4]);
        let b = store.put(vec![0; 4]);
        assert_ne!(a, b, "取件码不能撞，否则页面会放错音频");
    }
}
