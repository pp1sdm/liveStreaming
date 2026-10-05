//! 演示题库。
//!
//! 一道题就是一句韩文加四个中文选项，没有别的字段。题干刻意从单词一路排到
//! 短句——这两种长度在画面上占的行数差很多，出题时必须两种都有，排版才不会
//! 只在短词上看着对。
//!
//! 真正的题库以后从 CSV/JSON 导入，这里先手写一场够用的量。

use livestream_core::{Choice, Question};

fn q(id: u64, prompt: &str, options: [&str; 4], correct: Choice) -> Question {
    Question {
        id,
        prompt: prompt.to_owned(),
        options: options.map(str::to_owned),
        correct,
    }
}

/// 单词和短句混排，答案在 A B C D 上均匀分布。
pub fn demo_bank() -> Vec<Question> {
    vec![
        // ---------- 单词 ----------
        q(1, "학교", ["图书馆", "学校", "教室", "操场"], Choice::B),
        q(2, "물", ["水", "火", "土", "风"], Choice::A),
        q(3, "사랑", ["友情", "亲情", "爱情", "同情"], Choice::C),
        q(4, "시간", ["空间", "天气", "世界", "时间"], Choice::D),
        q(5, "먹다", ["喝", "吃", "看", "听"], Choice::B),
        q(6, "예쁘다", ["漂亮", "难看", "高大", "瘦小"], Choice::A),
        q(7, "감사합니다", ["对不起", "没关系", "谢谢", "再见"], Choice::C),
        q(8, "선생님", ["学生", "医生", "律师", "老师"], Choice::D),
        q(9, "어디", ["谁", "哪里", "什么", "为什么"], Choice::B),
        q(10, "내일", ["昨天", "今天", "明天", "后天"], Choice::C),
        q(11, "친구", ["朋友", "家人", "同事", "邻居"], Choice::A),
        q(12, "집", ["学校", "公司", "商店", "家"], Choice::D),
        q(13, "책", ["报纸", "书", "杂志", "地图"], Choice::B),
        q(14, "돈", ["钱", "票", "卡", "包"], Choice::A),
        q(15, "날씨", ["时间", "季节", "天气", "温度"], Choice::C),
        q(16, "아침", ["中午", "下午", "晚上", "早上"], Choice::D),
        q(17, "병원", ["药店", "医院", "银行", "邮局"], Choice::B),
        q(18, "가족", ["家人", "同事", "同学", "朋友"], Choice::A),
        q(19, "회사", ["工厂", "商店", "公司", "学校"], Choice::C),
        q(20, "노래", ["舞蹈", "电影", "故事", "歌"], Choice::D),
        // ---------- 短句。选项是另外几种译法，意思差一点，不认得就选不出来。 ----------
        q(
            21,
            "오늘 날씨가 좋아요.",
            ["今天很忙", "今天天气很好", "今天下雨", "今天很冷"],
            Choice::B,
        ),
        q(
            22,
            "저는 학생이에요.",
            ["我是学生", "我是老师", "我是医生", "我是职员"],
            Choice::A,
        ),
        q(
            23,
            "밥을 먹었어요.",
            ["正在做饭", "想吃饭", "吃过饭了", "要去吃饭"],
            Choice::C,
        ),
        q(
            24,
            "어디에 가요?",
            ["你是谁", "你几岁", "你什么时候来", "你去哪儿"],
            Choice::D,
        ),
        q(
            25,
            "이것은 얼마예요?",
            ["这是什么", "这个多少钱", "这个好吃吗", "这个在哪里"],
            Choice::B,
        ),
        q(
            26,
            "만나서 반갑습니다.",
            ["见到你很高兴", "好久不见", "请多关照", "我先走了"],
            Choice::A,
        ),
        q(
            27,
            "저는 한국어를 공부해요.",
            ["我在学中文", "我喜欢韩国", "我在学韩语", "我要去韩国"],
            Choice::C,
        ),
        q(
            28,
            "시간이 없어요.",
            ["没有钱", "没有胃口", "没有问题", "没有时间"],
            Choice::D,
        ),
        q(29, "물 좀 주세요.", ["请给我菜单", "请给我水", "请结账", "请慢用"], Choice::B),
        q(
            30,
            "내일 다시 만나요.",
            ["明天再见", "今天就到这里", "以后常联系", "下周见"],
            Choice::A,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bank_is_non_empty_and_ids_are_unique() {
        let bank = demo_bank();
        assert!(!bank.is_empty());

        let mut ids: Vec<u64> = bank.iter().map(|q| q.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), bank.len(), "题目 id 必须唯一");
    }

    #[test]
    fn no_duplicate_options_within_a_question() {
        // 选项重复会让「正确答案是 B」变得没有意义。
        for question in demo_bank() {
            let mut opts = question.options.to_vec();
            opts.sort_unstable();
            opts.dedup();
            assert_eq!(opts.len(), 4, "第 {} 题有重复选项", question.id);
        }
    }

    #[test]
    fn correct_answers_are_spread_across_all_four() {
        // 正确答案全挤在一个字母上的话，观众闭眼蒙同一个就能刷榜。
        let bank = demo_bank();
        for choice in Choice::ALL {
            assert!(
                bank.iter().any(|q| q.correct == choice),
                "{:?} 从来没做过正确答案",
                choice
            );
        }
    }

    #[test]
    fn bank_covers_both_words_and_sentences() {
        // 单词和短句在画面上占的行数差很多。演示题库要是只剩短词，长题干的
        // 排版问题就一直看不出来。
        let bank = demo_bank();
        let chars = |s: &str| s.chars().count();

        assert!(
            bank.iter().any(|q| chars(&q.prompt) <= 4),
            "至少要有一道单词题"
        );
        assert!(
            bank.iter().any(|q| chars(&q.prompt) >= 12),
            "至少要有一道短句题"
        );
    }

    #[test]
    fn prompts_are_korean_and_options_are_chinese() {
        // 题目是韩文、选项是中文，这个约定错了整个直播就不成立了。
        let is_hangul = |c: char| ('\u{AC00}'..='\u{D7A3}').contains(&c);
        let is_han = |c: char| ('\u{4E00}'..='\u{9FFF}').contains(&c);

        for question in demo_bank() {
            assert!(
                question.prompt.chars().any(is_hangul),
                "第 {} 题的题干里没有韩文：{}",
                question.id,
                question.prompt
            );
            for opt in &question.options {
                assert!(
                    opt.chars().any(is_han),
                    "第 {} 题有个选项不是中文：{}",
                    question.id,
                    opt
                );
            }
        }
    }
}
