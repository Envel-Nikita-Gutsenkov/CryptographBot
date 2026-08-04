pub fn build_full_prompt(
    symbol: &str,
    market_data: &str,
    news: &str,
    portfolio_section: &str,
) -> String {
    format!(
        "You are a professional cryptocurrency analyst.\n\
         Analyze market data and news for {symbol}.{portfolio_section}\n\n\
         Market Data:\n{market_data}\n\n\
         News:\n{news}\n\n\
         Your response MUST include these sections:\n\
         <b>Technical Analysis</b>: Trends, chart patterns, key support/resistance.\n\
         <b>Sentiment & News</b>: News impact and fear/greed index.\n\
         <b>Forecast</b>: State clearly — Bullish / Bearish / Neutral. Give a probability estimate \
         (e.g. 65% bullish) and a timeframe in days or weeks (e.g. next 1–3 weeks). Explain reasoning.\n\
         <b>Outlook</b>: Short/medium-term price targets and risk levels.\n\
         <b>Portfolio Strategy</b>: Advise specifically: Is this a good time to buy the dip, accumulate, \
         take profits, or hold? Provide actionable insight based on the portfolio context if available. \
         (Assume a manual long-term spot holding strategy. Do NOT advise on short positions, leverage, or stop-losses.)\n\
         <b>Rating</b>: Overall score (e.g. 7/10) or label (Bullish/Neutral/Bearish) + one-line reason.\n\n\
         Telegram Rich Markdown Formatting Rules:\n\
         - CRITICAL: Respond in the same language as the user's request.\n\
         - CRITICAL: NEVER use the `$` symbol for currencies (it breaks Telegram LaTeX parsing). Use 'USD' instead (e.g., 59,000 USD).\n\
         - Separate each section with a blank line (double newline).\n\
         - Use standard Markdown (`**bold**`, `*italic*`).\n\
         - Feel free to use Markdown tables (`| Col1 | Col2 |`) to present data beautifully.\n\
         - Use standard Markdown lists (`- `, `* `, `1. `). NEVER use raw unicode bullet characters.\n\
         - Use `#`, `##`, `###` for headings.\n\
         - Total response up to 30000 characters."
    )
}

pub fn build_multi_prompt(assets_data: &str, portfolio_section: &str) -> String {
    format!(
        "You are an expert crypto portfolio allocator.\n\
         Analyze the following assets for spot investment and accumulation.{portfolio_section}\n\n\
         Assets Data:\n{assets_data}\n\n\
         Your response MUST include:\n\
         <b>Market Overview</b>: Deep summary of current market conditions.\n\
         <b>Asset Comparison</b>: Detailed comparison of the provided assets (strength, weakness, opportunity).\n\
         <b>Accumulation Strategy</b>: Explicit and reasoned advice on which of these assets offer the best risk/reward for spot accumulation RIGHT NOW. Should the user buy, wait, or shift allocations? (Focus strictly on long-term spot holding. No leverage, no shorting, no stop-loss advice.)\n\
         <b>Top Pick</b>: The single best asset to buy today from this list and why.\n\n\
         Telegram Rich Markdown Formatting Rules:\n\
         - CRITICAL: Respond in the same language as the user's request.\n\
         - CRITICAL: NEVER use the `$` symbol for currencies (it breaks Telegram LaTeX parsing). Use 'USD' instead (e.g., 59,000 USD).\n\
         - Provide deep, informative, and reasoned analysis.\n\
         - Separate each section with a blank line (double newline).\n\
         - Use standard Markdown (`**bold**`, `*italic*`).\n\
         - Feel free to use Markdown tables (`| Col1 | Col2 |`) to present data beautifully.\n\
         - Use standard Markdown lists (`- `, `* `, `1. `). NEVER use raw unicode bullet characters.\n\
         - Use `#`, `##`, `###` for headings.\n\
         - Total response up to 30000 characters."
    )
}

pub fn build_watcher_prompt(
    symbols_data: &str,
    previous_analysis: Option<&str>,
    current_date: &str,
) -> String {
    let context_section = previous_analysis
        .map(|ctx| format!("\n\nPREVIOUS ANALYSIS CONTEXT:\nWhat you advised 6 hours ago:\n\"{ctx}\"\nCRITICAL RULE: Do NOT flip your opinion entirely (e.g., from strongly bullish to strongly bearish) within a short timeframe unless the market data clearly shows a massive breaking change. Maintain consistency and reliability with your previous advice. If the situation hasn't changed significantly, you may conclude it's 'none' to avoid spamming."))
        .unwrap_or_default();

    format!(
        "You are an active crypto market monitor. Analyze this market snapshot and determine if there is \
         something noteworthy that a portfolio holder should know.{context_section}\n\n\
         IMPORTANT SYSTEM INFO:\n\
         Today's Date: {current_date}\n\n\
         Market Data:\n{symbols_data}\n\n\
         Respond ONLY in this JSON format (no extra text):\n\
         {{\"severity\":\"none|important|critical\",\
          \"headline\":\"one short, casual sentence\",\
          \"detail\":\"1-2 short, conversational sentences with a simple action suggestion\"}}\n\n\
         Instructions for content (CRITICAL):\n\
         - Tone: Use a CASUAL, direct, and conversational tone, like a friend texting them. DO NOT use formal institutional language, jargon (like 'macro headwinds', 'capital rotation', 'institutional deposits'). Keep it simple and human (e.g., 'Bitcoin dropped 5%, you might want to buy the dip').\n\
         - Strategy: Assume a manual long-term spot holding strategy. DO NOT suggest short selling, leverage, or stop losses.\n\
         - Length: Keep it very short. Just tell them what happened and what they can do.\n\
         - SPAM PREVENTION: Only notify if there's actually something big happening! If the market is just moving normally (less than 5-10% changes) and there's no major news, ALWAYS return severity 'none'. People hate spam. Be silent unless it's truly important.\n\
         - Specifics: Mention the exact symbols, current prices, and precise percentage changes.\n\n\
         Use:\n\
         - critical: extreme move (>15% in 24h) or flash crash\n\
         - important: notable move (5-15%) or big news\n\
         - none: normal boring market activity (<5% changes), no action needed. DEFAULT to this."
    )
}

#[derive(Debug)]
pub struct WatcherResult {
    pub severity: WatcherSeverity,
    pub headline: String,
    pub detail: String,
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum WatcherSeverity {
    None,
    Important,
    Critical,
}

pub fn parse_watcher_response(raw: &str) -> WatcherResult {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) {
        let severity = match v["severity"].as_str().unwrap_or("none") {
            "critical" => WatcherSeverity::Critical,
            "important" => WatcherSeverity::Important,
            _ => WatcherSeverity::None,
        };
        return WatcherResult {
            severity,
            headline: v["headline"].as_str().unwrap_or("").to_string(),
            detail: v["detail"].as_str().unwrap_or("").to_string(),
        };
    }
    WatcherResult {
        severity: WatcherSeverity::None,
        headline: String::new(),
        detail: String::new(),
    }
}

/// Builds the Writer (generator) prompt for /patch.
/// The AI receives a free-text user request and a JSON snapshot of the portfolio,
/// and must respond with a strict PortfolioPatch JSON object.
pub fn build_patch_generator_prompt(user_request: &str, portfolio_snapshot: &str) -> String {
    format!(
        "You are a precise crypto portfolio data editor. The user gave you a free-text request \
         to modify their portfolio. Your job is to translate it into a structured JSON patch.\n\n\
         USER REQUEST:\n\"{user_request}\"\n\n\
         CURRENT PORTFOLIO SNAPSHOT (JSON):\n{portfolio_snapshot}\n\n\
         You MUST respond ONLY with a valid JSON object matching this exact schema (no extra text, no markdown):\n\
         {{\
           \"add_purchases\": [\
             {{\"symbol\": \"BTC\", \"amount\": 0.1, \"cost_usd\": 10000.0, \"wallet_name\": \"ledger\", \"source\": \"binance\", \"notes\": null}}\
           ],\
           \"remove_purchase_ids\": [12, 15],\
           \"adjust_holdings\": [\
             {{\"symbol\": \"ETH\", \"wallet_name\": \"exchange\", \"new_amount\": 1.5}}\
           ],\
           \"modify_purchases\": [\
             {{\"purchase_id\": 3, \"new_amount\": 0.05, \"new_cost_usd\": null, \"new_source\": null, \"new_notes\": null}}\
           ],\
           \"description\": \"Human-readable summary of what this patch does and why, in the same language the user used.\"\
         }}\n\n\
         RULES:\n\
         - Use ONLY the operations necessary to fulfill the request. If nothing needs to change for a field, leave the array empty.\n\
         - \"wallet_name\" and \"source\" may be null if unknown or not relevant.\n\
         - \"cost_usd\" in add_purchases is the TOTAL cost (not per unit). If unknown, estimate or set to 0.\n\
         - Never invent purchase IDs; only reference IDs that exist in the snapshot.\n\
         - \"description\" must be in the same language the user wrote their request in.\n\
         - If the user specifies a global portfolio cost or change (e.g., \"I spent $1550 on all crypto\") without specifying which exact coins it applies to, DO NOT leave the patch empty. Instead, mathematically distribute this new total cost proportionally across their EXISTING purchases in the snapshot to make the new total match their requested amount. Use `modify_purchases` to set the `new_cost_usd` for each existing purchase.\n\
         - Do NOT wrap in markdown. Return ONLY the raw JSON object."
    )
}

/// Builds the Critic (validator) prompt for /patch.
pub fn build_patch_critic_prompt(
    user_request: &str,
    portfolio_snapshot: &str,
    draft_patch: &str,
) -> String {
    format!(
        "You are a Senior Portfolio Data Auditor. Your job is to verify a proposed portfolio patch \
         for correctness, safety, and faithfulness to the user's original request.\n\n\
         ORIGINAL USER REQUEST:\n\"{user_request}\"\n\n\
         CURRENT PORTFOLIO SNAPSHOT:\n{portfolio_snapshot}\n\n\
         PROPOSED PATCH (JSON):\n{draft_patch}\n\n\
          AUDIT CHECKLIST:
          1. Does the patch faithfully implement ONLY what the user asked for?
          2. Are any purchase IDs being removed actually present in the snapshot?
          3. Are amounts/costs numerically reasonable (not negative, not wildly off)?
          4. Is the description accurate and matches what the patch actually does?
          5. Is the JSON schema valid and complete?

          RULES FOR WALLETS:
          - The user is not required to specify a wallet name. If \"wallet_name\" is null or absent in the patch, it is perfectly fine and safe (the system will automatically default it to their active wallet). Do NOT reject the patch for missing or null \"wallet_name\".

          Respond EXACTLY in this JSON format (no extra text):
          {{\"approved\": true,
            \"critique\": \"\",
            \"improved_text\": \"<the final patch JSON here>\"}}

          If the patch is correct, set approved=true and copy the original patch into improved_text.
          If it has issues, set approved=false, explain in critique, and put your corrected version in improved_text.
          IMPORTANT: improved_text must contain the raw PortfolioPatch JSON — not wrapped in markdown.
          Return ONLY valid JSON."
    )
}

pub fn build_ask_prompt(
    question: &str,
    portfolio_context: &str,
    market_data: &str,
    market_intel_context: &str,
    news_context: &str,
    web_search: &str,
    current_date: &str,
    reply_context: Option<&str>,
    memory_context: &str,
    preferred_lang: &str,
) -> String {
    let reply_str = match reply_context {
        Some(r) => format!("PREVIOUS AI RESPONSE (The user is replying to this):\n\"{r}\"\n\n"),
        None => String::new(),
    };

    let intel_section = if market_intel_context.trim().is_empty() {
        String::new()
    } else {
        format!("ON-CHAIN & MARKET INTELLIGENCE:\n{market_intel_context}\n\n")
    };

    let memory_section = if memory_context.trim().is_empty() {
        String::new()
    } else {
        format!("AI MEMORY & ACTIVE GOALS (the user's standing instructions — always respect them and factor them into advice):\n{memory_context}\n\n")
    };

    let web_section = if web_search.trim().is_empty() {
        String::new()
    } else {
        format!("## \u{1f310} LIVE WEB SEARCH RESULTS (fetched just now — treat as ground truth, prefer them over stale training knowledge):\n{web_search}\n\n")
    };

    format!(
        "You are a personal AI financial advisor for a crypto investor. \
         You have full knowledge of their portfolio, current market conditions, and recent news.\n\n\
         IMPORTANT SYSTEM INFO:\n\
         Today's Date: {current_date}\n\
         Use this date to understand the timeline of recent news and market events.\n\n\
         PORTFOLIO:\n{portfolio_context}\n\n\
         CURRENT MARKET DATA:\n{market_data}\n\n\
         {intel_section}\
         {memory_section}\
         RECENT NEWS:\n{news_context}\n\n\
         {web_section}\
         {reply_str}\
         USER QUESTION:\n\"{question}\"\n\n\
         Answer the user's question directly, accurately, and thoroughly. \
         If they ask about general news, market situations, or specific coins, leverage the provided news and market data. \
         Do NOT limit your answer only to their portfolio details unless the question is strictly about their holdings. \
         If the question doesn't relate to their portfolio (e.g. general news or coin traits), provide the information anyway, without complaining about missing portfolio items. \
         (Assume the user is a manual long-term spot investor. Do not mention short selling, leverage trading, or stop losses.)\n\n\
         CRITICAL DATA FIDELITY & NO-MATH COMPUTATION RULE:\n\
         Rely STRICTLY on the actual pre-calculated numbers provided in CURRENT MARKET DATA and ON-CHAIN & MARKET INTELLIGENCE (current price, 30d, 90d, 1y/365d, 2y/730d percentage changes, 1-2Y High/Peak ATH, Discount %, TVL, Funding Rates, Long/Short ratios, and Mempool fees).\n\
         DO NOT perform manual math calculations or re-estimate percentages; use the provided numbers as exact ground truth facts.\n\
         Your job is to act strictly as an ANALYST and STORYTELLER: explain what these calculated figures mean for the investor in plain language without re-calculating them.\n\n\
         Provide a detailed answer, BUT strictly focus on the user's explicit question. \
         CRITICAL CONTEXT RULE: Do NOT dump irrelevant macroeconomic or regulatory news into your response if the user's question is purely mathematical, technical, or focused on specific metrics (e.g. \"calculate discounts\"). Only use the News and Macro context if it directly answers the user's prompt. Avoid \"information bloat\".\n\n\
         If the user asks about an Ondo (RWA) token (like QQQON, SPYON, etc.), and you have TRADFI RWA ANCHORS data (like Real_Price_USD for QQQ), you MUST explicitly calculate and state the premium or discount of the tokenized asset vs the real asset (e.g. \"QQQON trades at a 0.15% premium to the real QQQ ETF\"). Assume the tokenized asset shares the long-term historical performance of its real-world counterpart; do not penalize it for lacking long-term on-chain metrics.\n\n\
         Telegram Rich Markdown Formatting:\n\
         - CRITICAL: {}\n\
         - CRITICAL: NEVER use the `$` symbol for currencies (it breaks Telegram LaTeX parsing). Use 'USD' instead (e.g., 59,000 USD).\n\
         - Use standard Markdown (`**bold**`, `*italic*`, `__underline__`).\n\
         - Feel free to use Markdown tables (`| Col1 | Col2 |`) to present data beautifully.\n\
         - Use standard Markdown lists (`- `, `* `, `1. `). NEVER use raw unicode bullet characters.\n\
         - Use `#`, `##`, `###` for headings.\n\
         - You have a huge 32000 character limit, so do not limit yourself.",
        lang_instruction(preferred_lang),
    )
}

pub fn build_moe_critic_prompt(
    prompt: &str,
    draft_a: &str,
    draft_b: &str,
    format_instruction: &str,
) -> String {
    format!(
        "You are a Senior Crypto Analyst Evaluator.\n\
        Original Request (which dictates format, ground truth market data, and rules):\n{prompt}\n\n\
        Draft Analysis A:\n{draft_a}\n\n\
        Draft Analysis B:\n{draft_b}\n\n\
        Evaluate both drafts. Check for hallucinations, illogical conclusions, incorrect historical dates, false price claims, or invented historical peak prices. You MUST fact-check all numbers, 1-2Y peak prices, and discount percentages against the GROUND TRUTH CURRENT MARKET DATA in the Original Request.\n\
        CRITICAL CHRONOLOGY CHECK: Check if the dates mentioned in the forecasts make logical sense relative to the \"Today's Date\" specified in the Original Request (e.g. predicting a peak in 2025 when today is 2026 is a hallucination). Do NOT allow time-traveling or outdated predictions. If they are outdated, rewrite them to align with the current date.\n\
        Your job is to pick the best draft, OR merge the best parts of both into a perfect unified analysis. If a draft hallucinates false peak prices or incorrect percentages, replace them with the exact figures from the market data. {format_instruction}\n\
        IMPORTANT: Wrap your complete final output inside <final_analysis> and </final_analysis> tags. Do NOT place any outline notes, bullet lists of planned changes, or meta-commentary inside the <final_analysis> block."
    )
}

/// Proactive goal-check prompt: the AI evaluates every active goal and memo
/// against the current market snapshot and returns strict JSON actions.
pub fn build_proactive_prompt(
    current_date: &str,
    portfolio_context: &str,
    market_snapshot: &str,
    market_intel: &str,
    news_context: &str,
    goals: &str,
    memos: &str,
    previous_analysis: Option<&str>,
    preferred_lang: &str,
) -> String {
    let prev = previous_analysis
        .map(|p| format!("PREVIOUS SCAN ANALYSIS (from the last scan; do not repeat alerts already sent unless conditions changed):\n{p}\n\n"))
        .unwrap_or_default();
    let intel = if market_intel.trim().is_empty() {
        String::new()
    } else {
        format!("ON-CHAIN & MARKET INTELLIGENCE:\n{market_intel}\n\n")
    };
    format!(
        "You are a proactive personal crypto investment assistant. You watch the market on behalf of a long-term spot investor and decide, for each goal, whether NOW is the right moment to nudge them with an alert.\n\n\
        IMPORTANT SYSTEM INFO:\n\
        Today's Date: {current_date}\n\
        Use this date to judge goal relevance (goals have created_at / expires_at timestamps).\n\n\
        PORTFOLIO:\n{portfolio_context}\n\n\
        CURRENT MARKET SNAPSHOT (ground truth — do NOT invent or re-calculate numbers):\n{market_snapshot}\n\n\
        {intel}\
        RECENT NEWS CONTEXT:\n{news_context}\n\n\
        {prev}\
        USER'S ACTIVE GOALS (each MUST appear exactly once in your output):\n{goals}\n\n\
        USER'S MEMORY NOTES (always respect them; they are rules the user set):\n{memos}\n\n\
        YOUR TASK:\n\
        For each goal decide one action:\n\
        - \"alert\": market conditions NOW strongly match the goal (e.g. a dip near a support zone while the goal is to buy the dip, or a x2 gain milestone reached). Write a concrete, actionable message for the user in their language with exact numbers from the snapshot (never the $ symbol, use USD).\n\
        - \"wait\": conditions do not justify a nudge yet. No message needed (reason may still be given).\n\
        - \"done\": the goal's target has clearly been achieved (e.g. x2 reached, planned spend completed, car money secured).\n\
        - \"stale\": the goal is no longer relevant (expired date, superseded by newer goals/memos, or impossible). Mark stale instead of alerting.\n\
        RULES:\n\
        - Be conservative: only alert when it would genuinely help. Prefer \"wait\" on weak signals. Do not spam.\n\
        - If a goal lists previous_alerts_sent, do NOT send an alert repeating the same point unless the situation materially changed since the last alert (new price levels, new events). When in doubt, choose \"wait\".\n\
        - Respect quiet periods: if the user is waiting for a discount, a -3% daily move is NOT a strong dip; a -10% or support-zone touch might be.\n\
        - Always respect the user's memory notes (e.g. fixed investment size, coins on hold).\n\
        - The user is a manual long-term spot investor: never suggest leverage, margin, or shorting.\n\
        - {} (this applies to alert messages and the market_read summary).\n\
        OUTPUT FORMAT — respond with ONE valid JSON object ONLY (no markdown fences, no commentary):\n\
        {{\n\
          \"market_read\": \"1-2 sentence summary of current market state for the user\",\n\
          \"goals\": [\n\
            {{\"goal_id\": <id>, \"action\": \"alert|wait|done|stale\", \"message\": \"only for alert/done/stale, else empty\", \"reason\": \"short explanation\"}}\n\
          ]\n\
        }}",
        lang_instruction(preferred_lang),
    )
}

/// Human-readable language instruction injected into prompts.
pub fn lang_instruction(code: &str) -> &'static str {
    match code {
        "en" => "RESPONSE LANGUAGE: always answer in English, regardless of the user's language",
        "ru" => {
            "RESPONSE LANGUAGE: всегда отвечай на русском языке, независимо от языка пользователя"
        }
        _ => "RESPONSE LANGUAGE: answer in the same language as the user",
    }
}

/// Cheap-model reviewer: verifies the two powerful drafts against the ground
/// truth snapshot and merges them into one corrected JSON draft.
pub fn build_proactive_reviewer_prompt(prompt: &str, draft_a: &str, draft_b: &str) -> String {
    format!(
        "You are a meticulous data verifier for a goal-check AI.\n\n\
        ORIGINAL REQUEST (contains the ground-truth market snapshot — verify every number against it):\n{prompt}\n\n\
        DRAFT A (by a powerful analyst):\n{draft_a}\n\n\
        DRAFT B (by another powerful analyst):\n{draft_b}\n\n\
        Your job:\n\
        1. Cross-check every price, percentage, and claim in both drafts against the ORIGINAL REQUEST's ground truth. Discard hallucinations and wrong numbers.\n\
        2. Merge the best parts of both into ONE corrected draft.\n\
        3. Make sure every goal_id from the ORIGINAL REQUEST appears EXACTLY once, and every action is one of: alert, wait, done, stale.\n\
        4. Do not invent new goals. Do not add meta-commentary.\n\
        Respond with ONE valid JSON object ONLY (no markdown fences)."
    )
}

/// Final critic: powerful model fact-checks the merged draft.
pub fn build_proactive_critic_prompt(prompt: &str, merged_draft: &str) -> String {
    format!(
        "You are the final fact-checking critic for a goal-check AI.\n\n\
        ORIGINAL REQUEST (ground truth snapshot and rules):\n{prompt}\n\n\
        DRAFT TO VERIFY:\n{merged_draft}\n\n\
        Verify EVERY number, percentage, and claim against the ground truth in the ORIGINAL REQUEST. Fix any hallucination or contradiction.\n\
        Ensure every goal_id present in the ORIGINAL REQUEST appears EXACTLY once, and every action is one of: alert, wait, done, stale.\n\
        Do NOT invent goals. Do NOT add meta-commentary, notes, or markdown fences.\n\
        Respond with ONE valid JSON object ONLY."
    )
}

#[allow(dead_code)]
pub fn build_news_judge_prompt(
    prompt_base: &str,
    prev_context: &str,
    writer_a: &str,
    draft_a: &str,
    writer_b: &str,
    draft_b: &str,
) -> String {
    format!(
        "You are the MoE (Mixture of Experts) Judge for Cryptocurrency News.\n\
         Original News Context:\n{prompt_base}\n\n\
         {prev_context}\n\
         Draft A (by {writer_a}):\n{draft_a}\n\n\
         Draft B (by {writer_b}):\n{draft_b}\n\n\
         Your task is to review both drafts, extract the best and most accurate facts, filter out any hallucinations or contradictory logic, and write the final ultimate bullet-point summary.\n\
         If both drafts indicate that the news is vague, insignificant, or insufficient to draw a strong conclusion, state 'No significant news detected' instead of inventing facts.\n\
         Output ONLY your final, perfect text (max 3-4 bullet points). No meta-commentary."
    )
}
