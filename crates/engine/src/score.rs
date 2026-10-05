//! 整场直播的积分榜。
//!
//! 计分只在**公布之后**发生。收答期间就算分，等于把「谁答对了」提前漏出去——
//! 榜单上冒出来的人名本身就是答案。
//!
//! 一场直播一份，不落盘。观众明天再来是新的榜，这符合「今天谁最强」的直播间
//! 惯例；真要跨场累计，那是 `store` 那层的事。

use std::collections::HashMap;

use livestream_core::{Answer, Choice};
use serde::Serialize;

/// 答对的基础分。
const BASE: u32 = 10;

/// 每轮最多留几条得分记录。榜上只需要前几名，留太多没地方摆。
const MAX_AWARDS: usize = 12;

/// 手速分：本轮第几个答对。
///
/// 前十名之外不再加分——直播间几百人同时刷「B」的时候，精确到毫秒的排名
/// 只是网络抖动，不该变成分数差。
fn speed_bonus(rank: usize) -> u32 {
    match rank {
        1 => 5,
        2..=3 => 3,
        4..=10 => 1,
        _ => 0,
    }
}

/// 连对加成。
///
/// 这是榜单上唯一会「加速」的东西：连对到第 3 题开始每题多拿一点，第 5 题
/// 起再多一点。目的是让人愿意每道题都答，而不是只答有把握的。
fn streak_bonus(streak: u32) -> u32 {
    match streak {
        0..=2 => 0,
        3..=4 => 2,
        _ => 5,
    }
}

/// 本轮某位答对者的得分明细。公布那一刻的反馈就是靠它。
#[derive(Debug, Clone, Serialize)]
pub struct Award {
    pub nickname: String,
    /// 本轮第几个答对，从 1 起。
    pub rank: u32,
    /// 本轮一共拿到多少分（含加成）。
    pub gained: u32,
    /// 其中的手速分。
    pub speed: u32,
    /// 其中的连对加成。
    pub streak_bonus: u32,
    /// 连对到第几题。
    pub streak: u32,
    /// 加完之后的总分。
    pub score: u32,
}

/// 一位观众的累计战绩。
#[derive(Debug, Clone, Serialize)]
pub struct Player {
    pub user_id: String,
    pub nickname: String,
    pub score: u32,
    pub correct: u32,
    /// 参与过几轮（答对答错都算）。
    pub answered: u32,
    /// 当前连对。答错清零，没答不算断。
    pub streak: u32,
    pub best_streak: u32,
    /// 第一次出现时的序号，用来给同分的人排序——先来的排前面。
    #[serde(skip)]
    joined: u64,
}

/// 榜单上的一行。
#[derive(Debug, Clone, Serialize)]
pub struct RankRow {
    /// 名次，从 1 起。
    pub rank: u32,
    pub nickname: String,
    pub score: u32,
    pub correct: u32,
    pub streak: u32,
}

/// 一场直播的积分榜。
#[derive(Debug, Default)]
pub struct Leaderboard {
    players: HashMap<String, Player>,
    /// 递增的发号器，保证同分排序稳定。
    seq: u64,
}

impl Leaderboard {
    pub fn new() -> Self {
        Self::default()
    }

    /// 结算一轮，返回本轮各人的得分明细。
    ///
    /// `answers` 是这一轮按到达顺序排好的有效作答。名次在这里重算，不信任
    /// 上游给的 `seq`——弹幕源的序号在重连之后可能会跳。
    pub fn record_round(&mut self, answers: &[Answer], correct: Choice) -> Vec<Award> {
        let mut awards = Vec::new();
        let mut rank = 0usize;

        for answer in answers {
            // 号先发出去再说。老观众不会用到这个值（他已经有 joined 了），
            // 白扔一个号无所谓——反正它只是个单调递增的排序依据。
            let joined = self.seq;
            self.seq += 1;

            let player = self.players.entry(answer.user_id.clone()).or_insert_with(|| Player {
                user_id: answer.user_id.clone(),
                nickname: answer.nickname.clone(),
                score: 0,
                correct: 0,
                answered: 0,
                streak: 0,
                best_streak: 0,
                joined,
            });

            // 观众能改昵称，以最后一次见到的为准。
            player.nickname.clone_from(&answer.nickname);
            player.answered += 1;

            if answer.choice != correct {
                // 答错断连对。没答不算断——人可能只是走开了。
                player.streak = 0;
                continue;
            }

            rank += 1;
            player.correct += 1;
            player.streak += 1;
            player.best_streak = player.best_streak.max(player.streak);

            let speed = speed_bonus(rank);
            let streak_bonus = streak_bonus(player.streak);
            let gained = BASE + speed + streak_bonus;
            player.score += gained;

            if awards.len() < MAX_AWARDS {
                awards.push(Award {
                    nickname: player.nickname.clone(),
                    rank: rank as u32,
                    gained,
                    speed,
                    streak_bonus,
                    streak: player.streak,
                    score: player.score,
                });
            }
        }

        awards
    }

    /// 前 `n` 名。只列答对过的人——一题没对也算上榜，榜就不好看了。
    pub fn top(&self, n: usize) -> Vec<RankRow> {
        let mut ranked: Vec<&Player> = self.players.values().filter(|p| p.correct > 0).collect();
        ranked.sort_by(|a, b| {
            b.score
                .cmp(&a.score)
                .then(b.correct.cmp(&a.correct))
                .then(a.joined.cmp(&b.joined))
        });
        ranked.truncate(n);

        ranked
            .into_iter()
            .enumerate()
            .map(|(i, p)| RankRow {
                rank: i as u32 + 1,
                nickname: p.nickname.clone(),
                score: p.score,
                correct: p.correct,
                streak: p.streak,
            })
            .collect()
    }

    /// 上榜人数（至少答对过一题的人）。
    pub fn ranked_count(&self) -> usize {
        self.players.values().filter(|p| p.correct > 0).count()
    }

    /// 累计参与人数。
    pub fn player_count(&self) -> usize {
        self.players.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answer(user: &str, choice: Choice, seq: u64) -> Answer {
        Answer {
            user_id: user.into(),
            nickname: user.into(),
            choice,
            seq,
        }
    }

    #[test]
    fn correct_answer_scores_base_plus_speed() {
        let mut lb = Leaderboard::new();
        let awards = lb.record_round(
            &[answer("first", Choice::B, 0), answer("second", Choice::B, 1)],
            Choice::B,
        );

        assert_eq!(awards[0].rank, 1);
        assert_eq!(awards[0].speed, 5);
        assert_eq!(awards[0].gained, BASE + 5);
        assert_eq!(awards[1].rank, 2);
        assert_eq!(awards[1].speed, 3);
        assert_eq!(awards[1].gained, BASE + 3);
    }

    #[test]
    fn speed_bonus_dries_up_past_tenth() {
        // 第十一名之后不再有手速分，但基础分照给。
        let answers: Vec<Answer> = (0..12)
            .map(|i| answer(&format!("u{i}"), Choice::A, i))
            .collect();
        let mut lb = Leaderboard::new();
        let awards = lb.record_round(&answers, Choice::A);

        assert_eq!(awards[9].speed, 1);
        assert_eq!(awards[10].speed, 0);
        assert_eq!(awards[11].gained, BASE);
    }

    #[test]
    fn rank_counts_only_correct_answers() {
        // 手速看的是「第几个答对」，不是「第几个说话」。
        let mut lb = Leaderboard::new();
        let awards = lb.record_round(
            &[
                answer("wrong1", Choice::A, 0),
                answer("wrong2", Choice::A, 1),
                answer("right", Choice::B, 2),
            ],
            Choice::B,
        );

        assert_eq!(awards.len(), 1);
        assert_eq!(awards[0].nickname, "right");
        assert_eq!(awards[0].rank, 1, "答错的人不该占掉手速名次");
    }

    #[test]
    fn streak_builds_up_and_pays_more() {
        let mut lb = Leaderboard::new();
        let a = answer("u1", Choice::A, 0);

        // 前两题只有基础分加手速分。
        for expected_streak in 1..=2 {
            let awards = lb.record_round(std::slice::from_ref(&a), Choice::A);
            assert_eq!(awards[0].streak, expected_streak);
            assert_eq!(awards[0].streak_bonus, 0);
        }
        // 第三题起有连对加成。
        let awards = lb.record_round(std::slice::from_ref(&a), Choice::A);
        assert_eq!(awards[0].streak, 3);
        assert_eq!(awards[0].streak_bonus, 2);
        assert_eq!(awards[0].gained, BASE + 5 + 2);
    }

    #[test]
    fn wrong_answer_resets_streak_but_keeps_score() {
        let mut lb = Leaderboard::new();
        let right = answer("u1", Choice::A, 0);

        lb.record_round(std::slice::from_ref(&right), Choice::A);
        lb.record_round(std::slice::from_ref(&right), Choice::A);
        let before = lb.top(1)[0].score;

        lb.record_round(&[answer("u1", Choice::D, 1)], Choice::A);
        let row = &lb.top(1)[0];

        assert_eq!(row.score, before, "答错不扣分");
        assert_eq!(row.streak, 0, "答错断连对");
        assert_eq!(row.correct, 2);
    }

    #[test]
    fn skipping_a_round_does_not_break_the_streak() {
        // 人走开一轮不算断，否则「每道题都必须猜一个」变成负担。
        let mut lb = Leaderboard::new();
        lb.record_round(&[answer("u1", Choice::A, 0)], Choice::A);
        lb.record_round(&[answer("other", Choice::A, 0)], Choice::A);

        let awards = lb.record_round(&[answer("u1", Choice::A, 0)], Choice::A);
        assert_eq!(awards[0].streak, 2);
    }

    #[test]
    fn board_excludes_players_who_never_got_one_right() {
        let mut lb = Leaderboard::new();
        lb.record_round(
            &[answer("right", Choice::A, 0), answer("alwayswrong", Choice::D, 1)],
            Choice::A,
        );

        let board = lb.top(10);
        assert_eq!(board.len(), 1);
        assert_eq!(board[0].nickname, "right");
        assert_eq!(lb.ranked_count(), 1);
        assert_eq!(lb.player_count(), 2, "参与人数还是要算上他");
    }

    #[test]
    fn ties_go_to_whoever_arrived_first() {
        let mut lb = Leaderboard::new();
        // 两个人各答对一题、都是第一名，分数一样。
        lb.record_round(&[answer("early", Choice::A, 0)], Choice::A);
        lb.record_round(&[answer("late", Choice::A, 0)], Choice::A);

        let board = lb.top(2);
        assert_eq!(board[0].nickname, "early");
        assert_eq!(board[1].nickname, "late");
        assert_eq!(board[0].score, board[1].score);
    }

    #[test]
    fn rank_column_is_dense_and_ordered() {
        let mut lb = Leaderboard::new();
        lb.record_round(
            &[
                answer("a", Choice::A, 0),
                answer("b", Choice::A, 1),
                answer("c", Choice::D, 2),
            ],
            Choice::A,
        );

        let board = lb.top(10);
        assert_eq!(board.iter().map(|r| r.rank).collect::<Vec<_>>(), [1, 2]);
        assert!(board[0].score >= board[1].score);
    }

    #[test]
    fn nickname_follows_the_latest_one_seen() {
        let mut lb = Leaderboard::new();
        lb.record_round(&[answer("u1", Choice::A, 0)], Choice::A);

        let mut renamed = answer("u1", Choice::A, 1);
        renamed.nickname = "改了名".into();
        lb.record_round(&[renamed], Choice::A);

        assert_eq!(lb.top(1)[0].nickname, "改了名");
        assert_eq!(lb.player_count(), 1, "同一个人不该占两行");
    }
}
