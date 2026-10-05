//! 台词：提示词、结构化输出、以及把数据拼成一段一段的话。
//!
//! # 为什么不全部交给模型写
//!
//! 讲解词里有一部分是**事实**：「12 人答对，8 人踩了 C」。这些数字在封盘
//! 之前根本不存在，所以不可能提前让模型写。而且让模型复述数字是自找麻烦——
//! 它会把 12 写成 13。
//!
//! 所以分成两层：
//!
//! 1. **AI 写的**：题干解释、为什么这个选项对（和题目一起生成，收答期就备好）
//! 2. **本地拼的**：人数、正确率、谁连对最多（封盘时用 Rust 拼字符串，零延迟、零错误）
//!
//! 过渡语（「请作答」「时间到」）既不依赖 AI 也不依赖实时数据，直接预生成成
//! 音频放进仓库，是最后的兜底——连 TTS 都挂了这个也还能响。

use serde::Deserialize;

use crate::tts::Segment;

/// 一句话的用途。用来决定它在什么阶段可以出现在画面上。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    /// 报题。收答期开头播。
    Intro,
    /// 公布·讲解。**含答案，公布前绝不能进画面。**
    Reveal,
    /// 弹幕互动。
    Chatter,
    /// 出错了以后顶上来的本地台词。
    Fallback,
}

/// 一句要念出来的话，按语言分好段，交给 TTS 逐段合成。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub kind: LineKind,
    pub segments: Vec<Segment>,
}

impl Line {
    pub fn new(kind: LineKind, segments: Vec<Segment>) -> Self {
        Self { kind, segments }
    }

    /// 纯文本形式。**只给日志和测试用**，不要拿它去合成——
    /// 合成必须走分段，否则韩语会被中文音色读歪。
    pub fn plain(&self) -> String {
        self.segments.iter().map(|s| s.text.as_str()).collect()
    }
}

/// 出题要模型吐的 JSON 结构。
///
/// 注意 DeepSeek 只支持 `json_object`、**不支持 `json_schema`**，
/// 所以服务端不会替我们校验字段。这些约束全靠提示词 + 下面的
/// [`GeneratedQuestion::validate`] 自己兜。
#[derive(Debug, Clone, Deserialize)]
pub struct GeneratedQuestion {
    /// 韩语题干，比如「다음 중 '학교'의 뜻은?」
    pub prompt: String,
    /// 四个中文选项。
    pub options: Vec<String>,
    /// 正确项，`"A"` 到 `"D"`。
    pub correct: String,
    /// 正确选项的中文意思。
    pub meaning_zh: String,
    /// 为什么是这个答案，一两句中文。**和题目一起生成**，收答期就备好。
    pub explanation_zh: String,
}

impl GeneratedQuestion {
    /// 校验模型吐出来的东西。这道关**必须**有，而且要严。
    ///
    /// 直播间在无人值守地跑，一道错题就是当着观众念错——而且没人能拦。
    /// 模型偶尔会：给三个选项、把答案标成 E、在选项里混韩语、
    /// 或者最阴的一种——`meaning_zh` 和它标为正确的那个选项**对不上**
    /// （标了 B 但解释的是 C 的意思）。最后这种不校验的话，
    /// 观众会看到「正确答案是 B」而 B 明显是错的。
    pub fn validate(&self) -> anyhow::Result<()> {
        use anyhow::bail;

        if self.prompt.trim().is_empty() {
            bail!("题干是空的");
        }
        if self.options.len() != 4 {
            bail!("选项必须是 4 个，收到 {}", self.options.len());
        }
        for (i, opt) in self.options.iter().enumerate() {
            if opt.trim().is_empty() {
                bail!("第 {} 个选项是空的", i + 1);
            }
            // 选项该是中文释义。混进韩语说明模型串了行。
            if opt.chars().any(is_hangul) {
                bail!("第 {} 个选项里有韩文，选项应该是中文释义：{opt}", i + 1);
            }
        }
        // 题干该是韩语。没有韩文说明模型出成了中文题——那就不是韩语教学了。
        if !self.prompt.chars().any(is_hangul) {
            bail!("题干里没有韩文，不像韩语题：{}", self.prompt);
        }
        // 选项之间不能重复，否则「两个都对」。
        for i in 0..4 {
            for j in (i + 1)..4 {
                if self.options[i].trim() == self.options[j].trim() {
                    bail!("选项 {} 和 {} 一样：{}", i + 1, j + 1, self.options[i]);
                }
            }
        }

        let idx = self.correct_index()?;

        // 自洽校验：标为正确的那个选项，必须就是 `meaning_zh` 说的意思。
        // 这是最要命的一类错误，必须挡住。
        let correct_text = &self.options[idx];
        if !similar(correct_text, &self.meaning_zh) {
            bail!(
                "自相矛盾：把 {} 标成正确答案（{}），但释义说的是「{}」",
                (b'A' + idx as u8) as char,
                correct_text,
                self.meaning_zh
            );
        }

        if self.explanation_zh.trim().is_empty() {
            bail!("解析是空的");
        }
        Ok(())
    }

    /// 正确项的序号。非法字母会在这里被挡下来。
    pub fn correct_index(&self) -> anyhow::Result<usize> {
        use anyhow::bail;
        let c = self.correct.trim().to_ascii_uppercase();
        let mut chars = c.chars();
        let (Some(ch), None) = (chars.next(), chars.next()) else {
            bail!("正确答案字段应该是单个字母，收到 {:?}", self.correct);
        };
        match ch {
            'A'..='D' => Ok(ch as usize - 'A' as usize),
            _ => bail!("正确答案只能是 A/B/C/D，收到 {ch}"),
        }
    }
}

/// 是不是韩文音节或字母。
fn is_hangul(ch: char) -> bool {
    matches!(ch as u32, 0xAC00..=0xD7A3 | 0x1100..=0x11FF | 0x3130..=0x318F)
}

/// 两段文字是不是在说同一件事。
///
/// 不做严格相等——模型写 `meaning_zh` 时常带「意思是」「（名词）」之类的
/// 包装，严格比对会误杀一堆好题。这里做的是"包含"：任一方包含另一方就算过。
/// 目的是抓住「标 B 但说的是 C」这种彻底错位，不是抓措辞差异。
fn similar(a: &str, b: &str) -> bool {
    let norm = |s: &str| {
        s.chars()
            .filter(|c| !c.is_whitespace() && !"「」『』（）()，,。.：:；;、".contains(*c))
            .collect::<String>()
    };
    let (a, b) = (norm(a), norm(b));
    if a.is_empty() || b.is_empty() {
        return false;
    }
    a.contains(&b) || b.contains(&a)
}

/// 出题用的系统提示词。
///
/// **必须逐字不变。** DeepSeek 的提示词缓存是按前缀命中计费的，
/// 命中 $0.003/M 而未命中 $0.15/M——差 50 倍。把可变内容（第几题、
/// 出过什么）全放进 user 消息里，这段 system 就永远是缓存命中的那部分。
pub const QUESTION_SYSTEM: &str = r#"你是在为一场韩语教学直播出题。观众是中文母语的韩语初学者。

出一道四选一的韩语题，只输出 JSON，不要任何解释性文字，格式：

{
  "prompt": "韩语题干，必须是韩语",
  "options": ["选项1", "选项2", "选项3", "选项4"],
  "correct": "A",
  "meaning_zh": "正确选项的中文意思",
  "explanation_zh": "一两句中文，说清为什么"
}

硬性要求：
- prompt 必须是韩语，可以是单词、短语或短句。题目问它的意思、用法或正确的助词/词尾。
- 四个选项必须是中文，互不相同，长度相当，不要有一个明显比别的长。
- correct 只能是 "A"、"B"、"C"、"D" 之一。
- meaning_zh 必须**就是** options 里对应 correct 的那一项，一字不差。
- explanation_zh 用中文，面向初学者，别用语法术语堆砌。
- 干扰项要有迷惑性：同义近义、常见误用、形近词。不要放明显荒谬的选项。
- 不要重复出同一道题。"#;

/// 主播人设。**这是给 AI 的初始上下文定义**，每场直播开头就定下来。
///
/// 存在的意义是让整场的语气是**一个人**，而不是每次调用各自发挥。
/// 出题、讲解、串场、互动全都带上这段，AI 才知道自己在什么节目里、
/// 对谁说话、该用什么口吻。没有它，模型会一会儿像老师、一会儿像客服。
pub const HOST_PERSONA: &str = r#"你是一档抖音韩语学习答题直播的主持人，全场由你一个人主持。

节目形式：你出一句韩语，屏幕上有四个中文选项，观众在评论区打 A/B/C/D 作答，
倒计时结束后你公布答案并讲解。你是唯一的主持人，没有人坐在后台。

你的风格：像个耐心的韩国语伴，不是老师。语气轻松、口语化，偶尔开个小玩笑，
但不开观众的玩笑。你为观众答对而高兴，也为他们答错而惋惜，但从不居高临下。
你说话短，一句话就是一整段，因为你说的话会变成语音念出来。

你不知道当前题目的答案——答案由系统在公布时告诉你。绝不猜测，绝不暗示。"#;

/// 弹幕互动用的系统提示词。
///
/// **注意这里给的不是弹幕原文，而是房间的整体状况**（见 [`RoomDigest`]）。
/// 你要的是"控场"——像一个真人主持那样感知现场气氛，然后说一句稳住节奏的话；
/// 不是逐条回复观众，更不是把弹幕念出来。
///
/// 把弹幕原文交给模型是有代价的：观众能看见 AI 说的话，所以任何一个人的
/// 发言都可能被 AI 放大成全场都听得到的声音，而现场没有人能拦。
/// 用摘要就没有这个问题——模型看到的是"有 12 个人在聊助词用法"，
/// 而不是具体某个人说了什么。
///
/// **这段里也绝不能出现当前题目或答案。** 观众能听见 AI 说话，
/// 只要能套出答案，公平性就没了。所以互动只在收答期做，
/// 而且提示词里根本不给它题目信息——它没有可泄漏的东西。
pub const CHATTER_SYSTEM: &str = r#"现在继续主持这档韩语答题直播。下面会给你此刻直播间的整体状况。

说一句中文，15 到 35 字，作为此刻的控场旁白。

硬性要求：
- 只输出这一句话本身，不要引号、不要前缀、不要表情符号。
- 不要提问，不要要求观众做任何事（问了也没人回应，反而显得冷场）。
- **不要提到任何观众说了什么具体内容**，你看到的只是统计，不是原话。
- 不要提到任何选项字母、题目内容或答案，你不知道也不猜。
- 不谈政治、宗教、疾病、外貌、年龄，不做任何评价性判断。
- 冷场（没什么人说话）时不要点破，说点鼓励的话把气氛托住就行。
- 人数少的时候别硬夸"好多人啊"，那样很假。"#;

/// 串场旁白用的系统提示词。用于**公布之后、下一题之前**的过渡。
pub const NARRATION_SYSTEM: &str = r#"现在继续主持这档韩语答题直播，你刚讲完上一题，马上要出下一题。

说一句中文，15 到 35 字，作为两题之间的串场。

硬性要求：
- 只输出这一句话本身，不要引号、不要前缀、不要表情符号。
- 不重复上一题的内容，也不预告下一题是什么（你还不知道）。
- 不要提问，不要要求观众做任何事。
- 目标是让节奏不停顿：承上启下，让人想接着答下一题。"#;

/// 此刻直播间的整体状况。**给模型的是这个，不是弹幕原文。**
///
/// 这是一次"分析"而不是"转发"：把几十条弹幕压成几个数字和一点点气氛描述，
/// 让模型据此判断该说什么。控场要的是判断，不是逐条回应。
#[derive(Debug, Clone, Default)]
pub struct RoomDigest {
    /// 本场第几题，从 1 起。
    pub index: usize,
    /// 已经有多少人作答。
    pub participants: u32,
    /// 离封盘还有多少秒。
    pub seconds_left: u64,
    /// 最近这段时间收到多少条弹幕。
    pub danmaku_count: usize,
    /// 其中有多少条看起来是在聊韩语本身（问用法、讨论词义等）。
    pub learning_count: usize,
    /// 去重后、去过答案痕迹的短样本。**已消毒**，可以直接给模型看。
    pub samples: Vec<String>,
}

impl RoomDigest {
    /// 拼成给模型看的 user 消息。
    pub fn prompt(&self) -> String {
        let mut out = format!("现在是你主持的第 {} 题。\n", self.index);
        out.push_str(&format!("还有 {} 秒封盘，已经有 {} 人作答。\n", self.seconds_left, self.participants));

        if self.danmaku_count == 0 {
            out.push_str("过去这一分钟评论区没什么人说话。\n");
        } else {
            out.push_str(&format!(
                "过去这一分钟评论区有 {} 条弹幕，其中 {} 条在聊韩语本身。\n",
                self.danmaku_count, self.learning_count
            ));
            if !self.samples.is_empty() {
                out.push_str("几条有代表性的（已匿名，仅供参考气氛）：\n");
                for s in self.samples.iter().take(5) {
                    out.push_str("  · ");
                    out.push_str(s);
                    out.push('\n');
                }
            }
        }
        out
    }
}

/// 一条弹幕能不能拿去给模型看。能就给出去掉答案痕迹的版本，不能就 `None`。
///
/// **这是防止答案从弹幕里绕回来的那道闸。** 观众会在评论区打字讨论答案
/// （"我觉得是B"），如果这些原话进了模型的上下文，模型就可能顺着说出
/// "大家都在讨论 B"——那等于替所有人公布了答案。
///
/// 同时它也让"念出观众原话"这件事不可能发生：模型拿不到未消毒的文本。
/// DeepSeek 确实有相当的判别力，但那是第二道防线；第一道应该是
/// **根本不把危险的东西递过去**，而不是指望它每次都拒绝。
pub fn sanitize_danmaku(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    // 纯选项字母（"A" / "b" / "选A" / "我选 A"）本身就是作答，不是聊天。
    let letters: String = trimmed
        .chars()
        .filter(|c| c.is_ascii_alphabetic())
        .collect::<String>()
        .to_ascii_uppercase();
    let only_choice_letters = !letters.is_empty() && letters.chars().all(|c| "ABCD".contains(c));
    if only_choice_letters && letters.len() <= 4 {
        return None;
    }

    // 提到答案、选项、对错的，一律丢掉——这类话里几乎必然含答案信息。
    const ANSWER_WORDS: [&str; 10] =
        ["答案", "选", "对不", "错了", "正确", "是不是", "应该是", "肯定", "我猜", "盲猜"];
    if ANSWER_WORDS.iter().any(|w| trimmed.contains(w)) {
        return None;
    }

    // 链接、广告、联系方式：既不该给模型看，也不该有机会被念出来。
    if trimmed.contains("http")
        || trimmed.contains("www.")
        || trimmed.contains('@')
        || trimmed.contains("加微")
        || trimmed.contains("私信")
    {
        return None;
    }

    // 太长的不像弹幕，像刷屏或注入尝试。
    let cleaned: String = trimmed.chars().take(40).collect();
    Some(cleaned)
}

/// 这条弹幕是不是在聊韩语学习本身。用来给模型一个"学习氛围"的信号。
pub fn looks_like_learning(raw: &str) -> bool {
    const HINTS: [&str; 8] =
        ["怎么", "为什么", "区别", "意思", "发音", "语法", "助词", "词尾"];
    HINTS.iter().any(|h| raw.contains(h)) || raw.chars().any(|c| matches!(c as u32, 0xAC00..=0xD7A3))
}

impl GeneratedQuestion {
    /// 变成引擎认的题目，**并把选项打乱**。
    ///
    /// # 为什么必须打乱
    ///
    /// 实测让模型出三道题，答案**全是 A**。这不是偶然——模型有很强的
    /// 「把正确答案放第一个」的偏好。这个偏好对这个直播是致命的：
    /// 观众看两轮就会发现规律，从此闭眼选 A 就能赢，韩语教学的意义就没了。
    ///
    /// 所以在服务端重排：先把四项随机打乱，再回头找正确答案跑到第几位。
    /// 模型爱把答案放哪儿都行，观众看到的分布是均匀的。
    ///
    /// `seed` 显式传进来而不是内部取时间，是为了可测——测试要能钉死
    /// 「某个种子一定得到某个排列」。
    pub fn into_question(self, id: u64, seed: u64) -> anyhow::Result<livestream_core::Question> {
        self.validate()?;
        let idx = self.correct_index()?;

        // Fisher-Yates。四项而已，但写成通用的没坏处。
        let mut order = [0usize, 1, 2, 3];
        let mut state = seed | 1; // 全零会让 xorshift 卡死在零
        for i in (1..4).rev() {
            let j = (xorshift(&mut state) % (i as u64 + 1)) as usize;
            order.swap(i, j);
        }

        // 原来的第 idx 项，打乱后跑到了哪个位置？
        let new_correct = order
            .iter()
            .position(|&orig| orig == idx)
            .expect("排列一定包含原来的每一位");

        let mut options: [String; 4] = Default::default();
        for (slot, &orig) in order.iter().enumerate() {
            options[slot] = self.options[orig].clone();
        }

        Ok(livestream_core::Question {
            id,
            prompt: self.prompt,
            options,
            correct: livestream_core::Choice::from_index(new_correct)
                .expect("新位置一定落在 A-D 内"),
        })
    }
}

/// xorshift64。打乱选项用，不值得为此引 `rand`。
fn xorshift(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    x
}

/// 把出题提示词的 user 部分拼出来。避开的题目从 `avoid` 传进来。
pub fn question_prompt(avoid: &[String], index: usize) -> String {
    let mut out = format!("这是本场第 {} 题。", index + 1);
    if !avoid.is_empty() {
        out.push_str("\n最近出过这些题，不要重复：\n");
        for q in avoid.iter().take(20) {
            out.push_str("- ");
            out.push_str(q);
            out.push('\n');
        }
    }
    out
}

/// 公布时**本地拼**的事实部分。零延迟、零错误，不经过模型。
///
/// 这一段是「最多人踩了 B」这类信息——它在封盘那一刻才存在，
/// 不可能提前生成。所以它必须由 Rust 拼，而不是让模型写。
pub struct RevealFacts<'a> {
    pub prompt: &'a str,
    pub meaning_zh: &'a str,
    pub correct: char,
    pub correct_text: &'a str,
    pub tally: [u32; 4],
    pub correct_count: u32,
    pub total: u32,
}

impl RevealFacts<'_> {
    /// 拼出事实部分的台词分段。
    pub fn segments(&self) -> Vec<Segment> {
        let mut segs = vec![
            Segment::zh("正确答案是"),
            Segment::zh(format!("{}，{}。", self.correct, self.correct_text)),
        ];

        if self.total == 0 {
            segs.push(Segment::zh("这一轮没有人作答。"));
        } else {
            segs.push(Segment::zh(format!(
                "{} 人作答，{} 人答对。",
                self.total, self.correct_count
            )));

            // 「最多人踩了哪个坑」——这是全场最有信息量的一句，
            // 但只有**它一个独占鳌头**时才说得出口。
            //
            // 这里原本是「在错选项里取最大」，于是 tally=[3,3,3,1]、正确是 A 时
            // 会念出「最多人选了 C，有 3 个人」——可 A 和 B 也是 3。
            // 观众盯着票数条就知道这句是假的。而且 `max_by_key` 在并列时返回
            // **最后**一个，所以同样一份票数，换个正确的选项就会念成 B 或 C，
            // 纯属随机。这行是本地拼的事实句，设计上就是「零延迟、零错误，
            // 不经过模型」——那就不能有这种错。
            //
            // 并列时宁可不说话：票数条就在屏幕上，不说话不算漏信息，
            // 说了假话才是。
            let idx = self.correct as usize - 'A' as usize;
            let top = self.tally.iter().copied().max().unwrap_or(0);
            let leaders: Vec<usize> = (0..4).filter(|i| self.tally[*i] == top).collect();

            // 独占最高票、是错的、而且不止一个人——三条都满足才值得说。
            if top >= 2 && leaders.len() == 1 && leaders[0] != idx {
                let letter = (b'A' + leaders[0] as u8) as char;
                segs.push(Segment::zh(format!(
                    "最多人选了 {}，有 {} 个人，这个容易混。",
                    letter, top
                )));
            }
        }

        // 韩语题干本身用韩语音色念一遍，让观众听到发音。
        segs.push(Segment::ko(self.prompt.to_owned()));
        // 没有释义就别说——否则念出来是「意思是「」。」，引号里空的。
        // 调用方本来该给值（见 `step_locked`），但这条是播音稿的最后一道门，
        // 空字符串不该有机会变成一句台词。
        if !self.meaning_zh.trim().is_empty() {
            segs.push(Segment::zh(format!("意思是「{}」。", self.meaning_zh)));
        }
        segs
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tts::Lang;

    fn good() -> GeneratedQuestion {
        GeneratedQuestion {
            prompt: "다음 중 '학교'의 뜻은?".to_owned(),
            options: vec!["学校".into(), "国家".into(), "朋友".into(), "天气".into()],
            correct: "A".to_owned(),
            meaning_zh: "学校".to_owned(),
            explanation_zh: "학교 是汉字词「学校」。".to_owned(),
        }
    }

    #[test]
    fn a_well_formed_question_passes() {
        good().validate().unwrap();
        assert_eq!(good().correct_index().unwrap(), 0);
    }

    #[test]
    fn rejects_the_mislabeled_answer() {
        // 最要命的一类：标 B 是正确，但释义写的是"学校"，而 B 是"国家"。
        // 不挡的话，直播会当着观众的面公布一个明显错的答案。
        let mut q = good();
        q.correct = "B".to_owned();
        let err = q.validate().unwrap_err().to_string();
        assert!(err.contains("自相矛盾"), "应该报自相矛盾，实际: {err}");
    }

    #[test]
    fn rejects_duplicate_options() {
        let mut q = good();
        q.options[1] = "学校".to_owned();
        assert!(q.validate().is_err(), "两个选项一样，等于两个都对");
    }

    #[test]
    fn rejects_wrong_option_count() {
        let mut q = good();
        q.options.pop();
        assert!(q.validate().is_err());
    }

    #[test]
    fn rejects_korean_in_options() {
        // 选项该是中文释义。混进韩语说明模型串行了。
        let mut q = good();
        q.options[0] = "학교".to_owned();
        q.meaning_zh = "학교".to_owned();
        assert!(q.validate().is_err());
    }

    #[test]
    fn rejects_a_prompt_without_korean() {
        // 没有韩文就不是韩语题。
        let mut q = good();
        q.prompt = "Which one means school?".to_owned();
        assert!(q.validate().is_err());
    }

    #[test]
    fn rejects_out_of_range_answer() {
        let mut q = good();
        q.correct = "E".to_owned();
        assert!(q.correct_index().is_err());
    }

    #[test]
    fn rejects_multi_letter_answer() {
        let mut q = good();
        q.correct = "AB".to_owned();
        assert!(q.correct_index().is_err());
    }

    #[test]
    fn shuffling_keeps_the_same_correct_answer() {
        // 打乱只换位置，不能把"哪个选项是对的"换掉。
        let q = good();
        let correct_text = q.options[0].clone(); // correct = "A"，而 A 是「学校」
        for seed in 0..300u64 {
            let core = q.clone().into_question(1, seed).unwrap();
            assert_eq!(
                core.option(core.correct),
                correct_text,
                "seed={seed} 时正确答案的文本被换掉了"
            );
        }
    }

    #[test]
    fn shuffling_keeps_exactly_the_same_four_options() {
        // 必须是个排列：四项一个不多一个不少。
        let q = good();
        let mut want: Vec<String> = q.options.clone();
        want.sort();
        for seed in 0..100u64 {
            let core = q.clone().into_question(1, seed).unwrap();
            let mut got: Vec<String> = core.options.to_vec();
            got.sort();
            assert_eq!(got, want, "seed={seed} 时选项集合变了");
        }
    }

    #[test]
    fn shuffling_spreads_answers_across_all_four_positions() {
        // **回归测试。** 实测让模型连出三道题，答案**全是 A**——模型强烈
        // 偏好把正确答案放第一位。不打乱的话，观众看两轮就发现规律，
        // 之后闭眼选 A 就能赢，这个韩语教学直播就废了。
        let q = good();
        let mut counts = [0usize; 4];
        for seed in 0..400u64 {
            let core = q.clone().into_question(1, seed).unwrap();
            counts[core.correct.index()] += 1;
        }
        for (pos, n) in counts.iter().enumerate() {
            assert!(
                *n > 40,
                "位置 {} 在 400 个种子里只出现 {} 次，分布太偏: {:?}",
                (b'A' + pos as u8) as char,
                n,
                counts
            );
        }
    }

    #[test]
    fn shuffling_refuses_an_invalid_question() {
        // 校验没过就不该进到打乱这一步。
        let mut q = good();
        q.correct = "B".to_owned(); // 和 meaning_zh 自相矛盾
        assert!(q.into_question(1, 42).is_err());
    }

    #[test]
    fn sanitizer_drops_pure_answers() {
        // 观众打的选项字母是作答，不是聊天。放进上下文等于把答案递给模型。
        for raw in ["A", "b", "C ", "选A", "我选 B", "ABCD"] {
            assert!(sanitize_danmaku(raw).is_none(), "{raw:?} 应该被丢掉");
        }
    }

    #[test]
    fn sanitizer_drops_anything_mentioning_the_answer() {
        // 这类话里几乎必然含答案信息，模型看到就可能说出来。
        for raw in [
            "我觉得答案是B",
            "这题选C吧",
            "是不是A啊",
            "肯定是学校",
            "我猜是那个",
            "盲猜D",
        ] {
            assert!(sanitize_danmaku(raw).is_none(), "{raw:?} 应该被丢掉");
        }
    }

    #[test]
    fn sanitizer_drops_ads_and_links() {
        // 既不该给模型看，也不该有任何机会被念出来。
        for raw in ["加微信领资料", "私信我", "http://spam.cn", "www.广告.com", "联系 a@b.com"] {
            assert!(sanitize_danmaku(raw).is_none(), "{raw:?} 应该被丢掉");
        }
    }

    #[test]
    fn sanitizer_keeps_ordinary_chatter() {
        // 正常的聊天要留下，否则互动就没什么可说的了。
        assert_eq!(sanitize_danmaku("  主播讲得真好  ").as_deref(), Some("主播讲得真好"));
        assert_eq!(sanitize_danmaku("학교 발음 예쁘다").as_deref(), Some("학교 발음 예쁘다"));
    }

    #[test]
    fn sanitizer_truncates_long_input() {
        // 超长的不是弹幕，像刷屏或者注入尝试。
        let long = "啊".repeat(200);
        let out = sanitize_danmaku(&long).unwrap();
        assert_eq!(out.chars().count(), 40);
        assert!(sanitize_danmaku("   ").is_none(), "空白应该丢掉");
    }

    #[test]
    fn learning_hint_spots_korean_and_questions() {
        assert!(looks_like_learning("这两个词有什么区别"));
        assert!(looks_like_learning("학교"));
        assert!(!looks_like_learning("哈哈哈"));
    }

    #[test]
    fn digest_says_so_when_the_room_is_quiet() {
        // 冷场时不能硬夸"好多人啊"，那很假。要让模型知道确实没人说话。
        let digest = RoomDigest { index: 3, participants: 1, seconds_left: 40, ..Default::default() };
        let text = digest.prompt();
        assert!(text.contains("没什么人说话"), "冷场要说出来: {text}");
        assert!(!text.contains("有代表性的"), "没样本就不该有样本段");
    }

    #[test]
    fn digest_reports_the_room_when_it_is_lively() {
        let digest = RoomDigest {
            index: 5,
            participants: 42,
            seconds_left: 12,
            danmaku_count: 87,
            learning_count: 30,
            samples: vec!["主播讲得真好".to_owned()],
        };
        let text = digest.prompt();
        assert!(text.contains("第 5 题"));
        assert!(text.contains("42 人作答"));
        assert!(text.contains("87 条弹幕"));
        assert!(text.contains("30 条在聊韩语"));
        assert!(text.contains("主播讲得真好"));
    }

    #[test]
    fn persona_tells_the_model_it_does_not_know_the_answer() {
        // 人设里必须钉死这一点：它不知道答案，所以不该猜。
        // 这是防泄题的最后一道语言层面的防线。
        assert!(HOST_PERSONA.contains("不知道当前题目的答案"));
        assert!(HOST_PERSONA.contains("绝不猜测"));
    }

    #[test]
    fn similar_tolerates_wrapping_but_catches_mismatch() {
        assert!(similar("学校", "意思是「学校」"));
        assert!(similar("学校（名词）", "学校"));
        assert!(!similar("学校", "国家"));
    }

    #[test]
    fn reveal_facts_mention_the_popular_wrong_choice() {
        // 「最多人选了哪个坑」只在真的有人扎堆选错时才说。
        let facts = RevealFacts {
            prompt: "학교",
            meaning_zh: "学校",
            correct: 'A',
            correct_text: "学校",
            tally: [3, 9, 1, 0],
            correct_count: 3,
            total: 13,
        };
        let text = facts.segments().iter().map(|s| s.text.clone()).collect::<String>();
        assert!(text.contains("13 人作答"), "要有总人数: {text}");
        assert!(text.contains("3 人答对"), "要有答对人数: {text}");
        assert!(text.contains("选了 B"), "9 个人选了 B，这是最有信息量的一句: {text}");
    }

    #[test]
    fn reveal_facts_stay_quiet_when_nobody_clustered() {
        // 只有一个人选错就不值得说"最多人踩了 B"——那听着像在点名。
        let facts = RevealFacts {
            prompt: "학교",
            meaning_zh: "学校",
            correct: 'A',
            correct_text: "学校",
            tally: [5, 1, 0, 0],
            correct_count: 5,
            total: 6,
        };
        let text = facts.segments().iter().map(|s| s.text.clone()).collect::<String>();
        assert!(!text.contains("最多人"), "一个人选错不该说「最多人」: {text}");
    }

    #[test]
    fn reveal_facts_stay_quiet_when_the_top_is_tied() {
        // **回归测试。** 线上真跑出来的一次：tally=[3,3,3,1]、正确是 A，
        // 嘴里念的是「最多人选了 C，有 3 个人」——可 A 和 B 也是 3。
        // 观众看着票数条就知道这句不对。
        //
        // 而且原来用的是 `max_by_key`，它在并列时返回**最后**一个，
        // 所以同样一份票数，正确答案换个位置就会念成 B 或 C：同一个事实，
        // 随机挑一个说出来。并列时闭嘴才对——票数条就在屏幕上。
        for correct in ['A', 'B', 'C', 'D'] {
            let facts = RevealFacts {
                prompt: "물",
                meaning_zh: "水",
                correct,
                correct_text: "水",
                tally: [3, 3, 3, 1],
                correct_count: 3,
                total: 10,
            };
            let text = facts.segments().iter().map(|s| s.text.clone()).collect::<String>();
            assert!(
                !text.contains("最多人"),
                "最高票并列时不该点名任何一个（correct={correct}）: {text}"
            );
            assert!(text.contains("10 人作答"), "其余事实照说: {text}");
        }
    }

    #[test]
    fn reveal_facts_stay_quiet_when_the_correct_answer_leads() {
        // 正确选项自己就是最高票，说明这题大家会——没有「坑」可讲。
        let facts = RevealFacts {
            prompt: "물",
            meaning_zh: "水",
            correct: 'A',
            correct_text: "水",
            tally: [7, 2, 0, 0],
            correct_count: 7,
            total: 9,
        };
        let text = facts.segments().iter().map(|s| s.text.clone()).collect::<String>();
        assert!(!text.contains("最多人"), "没人扎堆选错就别硬说: {text}");
    }

    #[test]
    fn reveal_facts_never_say_an_empty_meaning() {
        // **回归测试。** 线上真跑出来的：每一题公布都念「意思是「」。」——
        // 调用方把 `meaning_zh` 写成了空串，而 `segments()` 照样把它拼进了台词。
        // 空串不该有机会变成一句口播。
        let facts = RevealFacts {
            prompt: "학교",
            meaning_zh: "",
            correct: 'A',
            correct_text: "学校",
            tally: [3, 0, 0, 0],
            correct_count: 3,
            total: 3,
        };
        let text = facts.segments().iter().map(|s| s.text.clone()).collect::<String>();
        assert!(!text.contains("意思是"), "没有释义就别说话: {text}");
        assert!(!text.contains("「」"), "绝不能出现空的引号: {text}");
        // 只有空白也算没有。
        let blank = RevealFacts { meaning_zh: "   ", ..facts };
        let text = blank.segments().iter().map(|s| s.text.clone()).collect::<String>();
        assert!(!text.contains("「」"), "全空白同样是空: {text}");
    }

    #[test]
    fn reveal_facts_handle_an_empty_round() {
        let facts = RevealFacts {
            prompt: "학교",
            meaning_zh: "学校",
            correct: 'A',
            correct_text: "学校",
            tally: [0, 0, 0, 0],
            correct_count: 0,
            total: 0,
        };
        let text = facts.segments().iter().map(|s| s.text.clone()).collect::<String>();
        assert!(text.contains("没有人作答"), "空轮要有专门的台词: {text}");
    }

    #[test]
    fn reveal_facts_speak_korean_with_a_korean_segment() {
        // 题干必须单独成段并用韩语音色，否则会被中文音色读成汉字音。
        let facts = RevealFacts {
            prompt: "학교",
            meaning_zh: "学校",
            correct: 'A',
            correct_text: "学校",
            tally: [1, 0, 0, 0],
            correct_count: 1,
            total: 1,
        };
        let ko: Vec<_> = facts.segments().into_iter().filter(|s| s.lang == Lang::Ko).collect();
        assert_eq!(ko.len(), 1, "题干应该恰好有一个韩语分段");
        assert_eq!(ko[0].text, "학교");
    }
}
