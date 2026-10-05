//! 真打 DeepSeek 的接口。默认跳过，手动跑：
//!
//! ```bash
//! cargo test -p livestream-ai --test deepseek_live -- --ignored --nocapture
//! ```
//!
//! 这里验的是**最要命的那个假设**：模型真的会按我们要的结构吐 JSON 吗？
//! 提示词写了四条硬性要求，但 DeepSeek 只支持 `json_object`、不支持
//! `json_schema`，服务端**不会**替我们校验字段。所以要实测。
//!
//! 密钥从 `.env` 读，测试里不出现、也不打印。

use livestream_ai::deepseek::DeepSeek;
use livestream_ai::script::{self, GeneratedQuestion};

fn client() -> Option<DeepSeek> {
    let _ = dotenvy::dotenv();
    match DeepSeek::from_env() {
        Ok(Some(ds)) => Some(ds),
        Ok(None) => {
            eprintln!("跳过：没有 DEEPSEEK_API_KEY（.env 里没写或为空）");
            None
        }
        Err(err) => panic!("建客户端失败: {err}"),
    }
}

#[tokio::test]
#[ignore = "要联网 + 花钱，手动跑"]
async fn generates_questions_that_pass_validation() {
    let Some(ds) = client() else { return };

    // 连出 3 道。出一道通过可能只是运气——模型的合规率才是要看的指标。
    let mut passed = 0;
    for i in 0..3 {
        let raw = ds
            .chat_json(script::QUESTION_SYSTEM, &script::question_prompt(&[], i))
            .await
            .expect("调用失败");

        println!("\n--- 第 {} 道，原始返回 ---\n{}", i + 1, raw);

        let q: GeneratedQuestion = match serde_json::from_str(&raw) {
            Ok(q) => q,
            Err(err) => {
                println!("✗ JSON 都解不开: {err}");
                continue;
            }
        };

        match q.validate() {
            Ok(()) => {
                passed += 1;
                println!(
                    "✓ 通过：{} / 答案 {} = {} / 解析 {}",
                    q.prompt, q.correct, q.meaning_zh, q.explanation_zh
                );
            }
            Err(err) => println!("✗ 校验没过: {err}"),
        }
    }

    println!("\n3 道里通过 {passed} 道");
    // 不要求 100%——模型偶尔翻车是常态，所以流水线里必须有重试和降级。
    // 但如果三道全挂，说明提示词有问题，得改。
    assert!(passed >= 2, "合规率太低（{passed}/3），提示词需要调整");
}

#[tokio::test]
#[ignore = "要联网 + 花钱，手动跑"]
async fn chatter_stays_short_and_does_not_ask_questions() {
    let Some(ds) = client() else { return };

    let line = ds
        .chat(script::CHATTER_SYSTEM, "有观众在认真答题，随口鼓励一句")
        .await
        .expect("调用失败");
    let line = line.trim();
    println!("互动台词：{line}");

    assert!(!line.is_empty(), "不能是空的");
    // 提示词要求 20-40 字。允许松一点，但太短或太长都不像人话。
    let n = line.chars().count();
    assert!((8..=80).contains(&n), "长度 {n} 不像一句口播：{line}");
    assert!(!line.contains('？'), "不该提问——问了也没人回答：{line}");
    assert!(!line.contains('\n'), "该是一句话，不该分段：{line}");
}
