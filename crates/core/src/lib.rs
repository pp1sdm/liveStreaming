//! 领域模型。
//!
//! 这个 crate 只描述「一场答题直播里有哪些东西」，不做 IO、不认识抖音、
//! 也不知道弹幕从哪来。所有判定规则都以纯函数形式放在这里，方便单测。

use serde::{Deserialize, Serialize};

/// 选项标号。M0 锁定四选一。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Choice {
    A,
    B,
    C,
    D,
}

impl Choice {
    pub const ALL: [Choice; 4] = [Choice::A, Choice::B, Choice::C, Choice::D];

    /// 在这个题目的选项数组里的下标。
    pub fn index(self) -> usize {
        match self {
            Choice::A => 0,
            Choice::B => 1,
            Choice::C => 2,
            Choice::D => 3,
        }
    }

    pub fn letter(self) -> char {
        match self {
            Choice::A => 'A',
            Choice::B => 'B',
            Choice::C => 'C',
            Choice::D => 'D',
        }
    }

    pub fn from_index(i: usize) -> Option<Choice> {
        Choice::ALL.get(i).copied()
    }
}

/// 一道四选一的题。
///
/// 就两样东西：一句韩文，四个中文选项，只有一个对。韩文可以是一个单词
/// （`물`），也可以是一句短句（`오늘 날씨가 좋아요`）——出题时不用区别对待。
///
/// 题干和选项都是给人看的文本，判定层不认识它们——判定只看字母。这让韩文
/// 编码问题（NFC/NFD、全角）不会渗进运行时。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Question {
    pub id: u64,
    /// 韩文题干。单词或短句都行。
    pub prompt: String,
    /// 四个中文选项，顺序对应 A B C D。只有一个是对的。
    pub options: [String; 4],
    pub correct: Choice,
}

impl Question {
    pub fn option(&self, c: Choice) -> &str {
        &self.options[c.index()]
    }
}

/// 一条被采纳的作答。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Answer {
    pub user_id: String,
    pub nickname: String,
    pub choice: Choice,
    /// 到达顺序。用序号而不是墙钟时间排序，避免推送延迟造成的不公平。
    pub seq: u64,
}

impl Answer {
    pub fn is_correct(&self, q: &Question) -> bool {
        self.choice == q.correct
    }
}

/// 一条弹幕，归一化之后的样子。
///
/// 各个 `DanmakuSource` 实现负责把自家格式翻译成这个结构。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Danmaku {
    pub seq: u64,
    pub user_id: String,
    pub nickname: String,
    pub content: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn choice_index_roundtrips() {
        for c in Choice::ALL {
            assert_eq!(Choice::from_index(c.index()), Some(c));
        }
        assert_eq!(Choice::from_index(4), None);
    }

    #[test]
    fn question_looks_up_option_by_choice() {
        let q = Question {
            id: 1,
            prompt: "학교".into(),
            options: ["医院".into(), "学校".into(), "公司".into(), "家".into()],
            correct: Choice::B,
        };
        assert_eq!(q.option(Choice::B), "学校");
    }
}
