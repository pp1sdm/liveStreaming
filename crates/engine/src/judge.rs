//! 判定流水线。
//!
//! 全项目只有这一条规则：**把弹幕里所有 A–D 字符抽出来，恰好剩一个才算数。**
//!
//! 为什么这么严？因为真实直播间里含这几个字母的弹幕远不止答案：
//! `haha`（含两个 a）、`打卡`、`666`、以及各种带英文的闲聊都会被推过来。
//! 抽出多个字母时宁可不猜——漏掉一条误判为答案，会污染票数分布，
//! 而且这种污染是系统性的（`a` 在英文里太常见）。

use livestream_core::Choice;
use serde::Serialize;
use unicode_normalization::UnicodeNormalization;

/// 一条弹幕的判定结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// 恰好识别出一个选项，计为作答。
    Chosen(Choice),
    /// 不是一次有效作答。
    Rejected(Rejection),
}

/// 被丢弃的原因，会进「疑似」日志供主播回看调优。
///
/// `Ambiguous` 是结构体变体而不是 `Ambiguous(String)`：内部标记
/// （`#[serde(tag = "kind")]`）要求每个变体的内容都能序列化成 map，而 newtype
/// 包一个 `String` 序列化出来是字符串，serde 会在**运行时**才报错。
/// 原先那一版只在有人打出「抽出多个字母」的弹幕时才会炸，而它连带把会话锁
/// 弄脏，整个服务就再也起不来了。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Rejection {
    /// 一个选项字母都没有。这是绝大多数噪声弹幕。
    NoChoice,
    /// 抽出了多个选项字母，无法确定观众想选哪个。
    Ambiguous { letters: String },
}

impl Rejection {
    /// 给主播看的一句话。控制台把它直接当文案显示。
    pub fn describe(&self) -> String {
        match self {
            Self::NoChoice => "没有选项字母".to_owned(),
            Self::Ambiguous { letters } => format!("抽到多个字母 {letters}"),
        }
    }
}

/// 判定一条弹幕。
pub fn judge(raw: &str) -> Verdict {
    let letters: Vec<Choice> = normalize(raw).chars().filter_map(choice_of).collect();

    match letters.as_slice() {
        [only] => Verdict::Chosen(*only),
        [] => Verdict::Rejected(Rejection::NoChoice),
        many => Verdict::Rejected(Rejection::Ambiguous {
            letters: many.iter().map(|c| c.letter()).collect(),
        }),
    }
}

/// 归一化。
///
/// NFKC 一次性解决三件事：全角 `Ａ` → 半角 `A`（中文输入法很常打全角）、
/// 韩文 자모 的组合与分解形式折叠、以及各种兼容字符（`Ⓐ` 之类）折叠。
/// 顺手也把题库里可能混进来的 NFD 韩文规范掉了。
///
/// 注意顺序：先 NFKC 再转大写，否则 `ａ` 这种全角小写会漏掉。
fn normalize(raw: &str) -> String {
    raw.nfkc().collect::<String>().to_uppercase()
}

fn choice_of(c: char) -> Option<Choice> {
    match c {
        'A' => Some(Choice::A),
        'B' => Some(Choice::B),
        'C' => Some(Choice::C),
        'D' => Some(Choice::D),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chosen(raw: &str) -> Choice {
        match judge(raw) {
            Verdict::Chosen(c) => c,
            other => panic!("{raw:?} 应该被采纳，实际是 {other:?}"),
        }
    }

    fn rejected(raw: &str) -> Rejection {
        match judge(raw) {
            Verdict::Rejected(r) => r,
            other => panic!("{raw:?} 应该被丢弃，实际是 {other:?}"),
        }
    }

    #[test]
    fn bare_letters() {
        assert_eq!(chosen("A"), Choice::A);
        assert_eq!(chosen("b"), Choice::B);
        assert_eq!(chosen(" c "), Choice::C);
        assert_eq!(chosen("D"), Choice::D);
    }

    #[test]
    fn full_width_letters_fold() {
        // 中文输入法下很容易打出全角字母，这是最常见的「看着一样却判错」来源。
        assert_eq!(chosen("Ｂ"), Choice::B);
        assert_eq!(chosen("ａ"), Choice::A);
        assert_eq!(chosen("Ｄ"), Choice::D);
    }

    #[test]
    fn letters_with_surrounding_text() {
        assert_eq!(chosen("我选B"), Choice::B);
        assert_eq!(chosen("选a"), Choice::A);
        assert_eq!(chosen("A吧"), Choice::A);
        assert_eq!(chosen("答案是 C"), Choice::C);
        assert_eq!(chosen("정답은 D"), Choice::D);
        assert_eq!(chosen("我觉得是 B 吧"), Choice::B);
    }

    #[test]
    fn noise_without_any_choice() {
        assert_eq!(rejected("打卡"), Rejection::NoChoice);
        assert_eq!(rejected("666"), Rejection::NoChoice);
        assert_eq!(rejected("哈哈哈"), Rejection::NoChoice);
        assert_eq!(rejected(""), Rejection::NoChoice);
        assert_eq!(rejected("   "), Rejection::NoChoice);
        assert_eq!(rejected("안녕하세요"), Rejection::NoChoice);
    }

    #[test]
    fn multiple_letters_are_dropped_not_guessed() {
        // 这是模糊匹配之下最主要的误触发来源：haha 抽出来是两个 A。
        assert_eq!(rejected("haha"), Rejection::Ambiguous { letters: "AA".into() });
        // 改口句也丢掉。宁可漏一条，也不能把 haha 记成选了 A。
        assert_eq!(rejected("不是A是B"), Rejection::Ambiguous { letters: "AB".into() });
        assert_eq!(rejected("ABCD"), Rejection::Ambiguous { letters: "ABCD".into() });
        assert_eq!(rejected("ＡＢ"), Rejection::Ambiguous { letters: "AB".into() });
    }

    #[test]
    fn normalization_order_handles_full_width_lowercase() {
        // 如果先转大写再 NFKC，`ａ` 不会被折叠成 A。这条守住顺序。
        assert_eq!(chosen("ａ"), Choice::A);
        assert_eq!(chosen("ｄ"), Choice::D);
    }

    #[test]
    fn every_variant_serializes() {
        // 控制台会把 Rejection 塞进 JSON 回给前端，任何变体序列化失败都会
        // 在运行时 panic。`Ambiguous` 以前是 `Ambiguous(String)`，配上
        // `#[serde(tag = "kind")]` 正好是 serde 不支持的一种组合——内部标记
        // 要求变体内容能序列化成 map，而 newtype 包 String 序列化出来是字符串。
        // 这条守住它别再退化回去。
        for r in [
            Rejection::NoChoice,
            Rejection::Ambiguous { letters: "AA".into() },
        ] {
            let out = serde_json::to_string(&r);
            assert!(out.is_ok(), "{r:?} 序列化失败：{out:?}");
        }
    }

    #[test]
    fn describe_is_readable() {
        assert_eq!(Rejection::NoChoice.describe(), "没有选项字母");
        assert_eq!(
            Rejection::Ambiguous { letters: "AA".into() }.describe(),
            "抽到多个字母 AA"
        );
    }
}
