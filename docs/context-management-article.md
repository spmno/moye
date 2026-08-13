# 让AI Agent不再"失忆"：我给moye加了两层上下文压缩

大家好，我是老孙。

之前用DeepSeek V4 Flash跑Agent做坦克大战，跑了三百多万Token，最后只花了0.49元。但有个问题一直没说——Agent跑久了，上下文窗口会爆。

今天就聊这个：我怎么给自己写的Agent加了一套上下文管理系统，从调研到实现，踩了什么坑，最后怎么解决的。

TL;DR：两层压缩，第一层清旧工具结果（不花一分钱），第二层LLM摘要（花几分钱），Agent可以无限跑下去。

项目地址：https://github.com/spmno/moye

---

## 一、问题：Agent跑着跑着就"失忆"了

用过的朋友都知道，moye是一个自主循环的Agent——你给它一个目标，它自己规划、调工具、写代码、跑测试，中间不需要你管。

但跑个二三十轮之后，问题来了：

```
User: 帮我写一个完整的坦克大战
Assistant: 好的，我先规划一下... (规划了3轮)
Assistant: 开始写代码... (调了read_file看现有结构)
User: [ToolResult: 788行代码全部返回]          ← 占了大量token
Assistant: 我来重构... (调了run_bash跑cargo build)
User: [ToolResult: 编译输出50行]                ← 又占了一堆
Assistant: 写测试... (调了edit_file改了5个文件)
User: [ToolResult: 5个文件修改成功]
Assistant: 跑测试... (调了run_bash跑cargo test)
User: [ToolResult: 测试输出30行]                ← 又来
...20轮后...
💥 上下文窗口超了，API报错，Agent卡死
```

每一次工具调用的结果（ToolResult）都会留在历史里。`read_file`返回几百行代码、`run_bash`返回编译输出、`edit_file`返回修改确认——这些东西加起来，token涨得飞快。

128K的上下文窗口，跑个20轮就不够用了。

---

## 二、调研：主流方案都怎么做的

动手之前先看看别人怎么解决的。花了一天时间扒了几个主流Agent框架的源码和文档：

**Claude Code（Anthropic官方CLI）**：五层渐进式压缩。从清理旧消息到LLM摘要，一层一层往上压，很精细，但实现复杂。

**Codex CLI（OpenAI）**：服务端压缩。API自己帮你管上下文，客户端不用操心。简单，但黑盒，你不知道它压了什么。

**LangChain Deep Agents**：工具输出卸载。把大工具结果存到外部存储，历史里只留引用。省token，但需要额外存储基础设施。

**OpenCode**：模型驱动压缩。给Agent一个"Compress"工具，让它自己决定什么时候压缩、压缩什么。灵活，但Agent不一定靠谱。

**Google ADK**：滑动窗口。保留最近N轮对话，旧的直接丢。最简单，但丢信息。

研究完之后，我的结论是：

> **没有银弹。**每个方案都是在"保留信息"和"省token"之间做取舍。

最关键的问题是：**工具结果占了大头，但价值递减**。你20轮前`read_file`返回的代码，现在大概率不需要完整内容了。但20轮前用户说的"要用Canvas渲染"，这个你得记住。

所以方向就清楚了：**先清旧工具结果（便宜），不够再LLM摘要（贵但保信息）**。

---

## 三、V1：滚动摘要，先跑起来

第一版很简单：上下文快满了的时候，把旧对话切出来，扔给LLM生成一段摘要，替换掉旧消息。

### 整体架构

```
┌─────────────────────────────────────────────┐
│               ContextHook                   │
│        (挂在Agent循环上的Hook)               │
├─────────────────────────────────────────────┤
│                                             │
│  1. 估算token → 接近溢出？                   │
│     ├── 否 → 不处理，继续                    │
│     └── 是 → 进入压缩流程                    │
│                                             │
│  2. 切分历史                                 │
│     旧消息（压缩）  |  近期消息（保留）       │
│                                             │
│  3. 旧消息 → LLM摘要                         │
│     "压缩以下对话历史..."                     │
│                                             │
│  4. 返回: [摘要] + [近期消息]                │
│     通过rig的PatchRequest替换当轮历史         │
│                                             │
└─────────────────────────────────────────────┘
```

### 关键技术点

**1. Token估算，不用tokenizer**

装tokenizer要引入一堆依赖，而且不同模型的tokenizer还不一样。我用了一个字符级启发式：

```rust
fn estimate_tokens(text: &str) -> usize {
    // CJK字符：约2字符/token
    // 拉丁字符：约4字符/token
    // 每条消息额外4 token开销
    let cjk_tokens = (cjk_count + 1) / 2;
    let latin_tokens = (other_count + 3) / 4;
    cjk_tokens + latin_tokens
}
```

不准？确实不准。但够了。因为我还用了API返回的**真实token数**来校准——每轮API调用会返回`Usage`，里面有`input_tokens`，用这个真实值替换估算值，溢出检测就越来越准。

**2. 历史切分**

不是简单地从中间一刀切。要把"一轮对话"作为一个整体——一个User消息加上后面跟着的Assistant回复和ToolResult，直到下一个User消息。这样切出来的旧消息和近期消息都是完整的对话轮。

```rust
// 一轮 = 一条User消息（非ToolResult）+ 后续的Assistant和ToolResult
// 直到下一条User消息
pub fn split_history(history: &[Message], keep_recent: usize) 
    -> (Vec<Message>, Vec<Message>)
```

**3. rig的PatchRequest机制**

这是整个系统的基础。rig框架（我用的Rust LLM框架）提供了`Flow::PatchRequest`，可以替换当轮发给API的历史。

关键是：**这是每轮非粘性的**。只改当轮发给API的历史，rig内部持久化的真实transcript不受影响。Agent始终能访问完整历史，只是在发送给API时用压缩版替换。

```rust
async fn handle_completion_call(&self, history: &[Message], _turn: usize) -> Flow {
    let estimated = estimate_history_tokens(history);
    
    if !is_near_overflow(estimated) {
        return Flow::Continue;  // 没超，不处理
    }
    
    let (old, recent) = split_history(history, keep_recent_turns);
    let summary = compact_via_llm(&old).await;
    
    let compacted = vec![Message::system(summary)]
        .into_iter()
        .chain(recent)
        .collect();
    
    Flow::patch_request(RequestPatch::new().history(compacted))
}
```

V1上线了，能跑。但很快发现问题。

---

## 四、翻车：V1不够好

V1的问题主要有三个：

### 问题1：每次压缩都要调LLM，贵

上下文一满就得调一次摘要LLM。一个长任务跑下来，光压缩就调了七八次LLM，每次都要花token。

更气人的是，很多时候上下文只是**略微超了**——比如旧工具结果占了大头，但用户的核心指令就那几句。这种情况根本不需要LLM摘要，清掉旧工具结果就行了。

### 问题2：摘要模板太简单

V1的摘要提示词就一句话：

```
你是对话历史压缩器。将以下对话历史压缩为简洁的结构化摘要，
保留关键信息：用户目标、已完成的工作、工具调用结果摘要、
未完成的步骤、重要决策与发现。
用简洁的中文要点格式输出，不要超过 500 字。
```

LLM生成的摘要质量全看运气。有时候重点信息丢了（比如文件路径、错误信息），有时候废话一堆（比如"用户表达了积极的情绪"）。

### 问题3：缓存机制不够好

当Agent在等待用户输入或HITL确认时，`handle_completion_call`可能被多次调用。如果历史没变，每次都重新压缩，纯浪费。

---

## 五、V2：两层压缩，先省后花

想了一天，定下方案：**C+B组合策略**——多层压缩（C）+ 改进现有滚动摘要（B）。

### 整体策略

```
              估算token数
                  │
     ┌────────────┴────────────┐
     │                         │
  ≤ 20K tokens             > 20K tokens
     │                         │
  不处理                   Tier 1: 微压缩（无LLM）
  Continue                       │
                          ┌──────┴──────┐
                          │             │
                     仍超阈值       已够低
                          │             │
                     Tier 2: LLM    返回微压缩历史
                     摘要压缩
                          │
                     返回摘要+近期历史
```

核心思路：**能用便宜方法解决的，不调LLM。**

### Tier 1：微压缩，不花一分钱

这是V2最重要的一层。

思路很简单：扫描历史里所有`ToolResult`，保护最近3个（因为可能还需要参考），更早的工具结果内容直接替换成一个标记：

```
压缩前：
  User: [ToolResult: fn main() { ...200行代码... }]    ← 20轮前的read_file结果
  User: [ToolResult: cargo build ... 50行输出 ...]     ← 15轮前的编译输出
  User: [ToolResult: file edited successfully]           ← 最近的，保留
  User: [ToolResult: cargo test ... 30行输出 ...]       ← 最近的，保留
  User: [ToolResult: fn helper() { ... }]               ← 最近的，保留

压缩后：
  User: [ToolResult: [Tool result cleared]]             ← 替换，省几百token
  User: [ToolResult: [Tool result cleared]]             ← 替换，省几十token
  User: [ToolResult: file edited successfully]           ← 原样保留
  User: [ToolResult: cargo test ... 30行输出 ...]       ← 原样保留
  User: [ToolResult: fn helper() { ... }]               ← 原样保留
```

实现上有几个细节要注意：

**1. 只清内容，不删消息**

API需要`ToolResult`的`id`和`call_id`来关联工具调用。如果把整个消息删了，API会报错说工具调用没有对应结果。所以只替换内容，保留ID：

```rust
new_items.push(UserContent::ToolResult(ToolResult {
    id: tr.id.clone(),         // 保留ID
    call_id: tr.call_id.clone(), // 保留call_id
    content: OneOrMany::one(ToolResultContent::text(
        "[Tool result cleared / 工具结果已清除]",
    )),
}));
```

**2. 一个User消息里可能混了Text和ToolResult**

rig的`Message::User`的content是`OneOrMany<UserContent>`，一个User消息可以同时包含文本和工具结果。只清ToolResult，不动Text：

```rust
for (ci, item) in content.iter().enumerate() {
    if to_clear.contains(&(msg_idx, ci)) {
        // 只替换ToolResult
        if let UserContent::ToolResult(tr) = item { ... }
    } else {
        // Text和其他内容原样保留
        new_items.push(item.clone());
    }
}
```

**3. OneOrMany的重建**

rig的`OneOrMany`类型保证至少有一个元素。重建时要用`OneOrMany::many(vec).expect(...)`——我们知道原始消息至少有一个content item，所以这里不会panic：

```rust
let new_content = OneOrMany::many(new_items)
    .expect("non-empty: original message had at least one content item");
```

Tier 1是纯内存操作，零LLM调用，零成本。实测中，微压缩能砍掉30-50%的token——因为工具结果往往是历史中最大的部分。

### Tier 2：改进的LLM摘要

Tier 1不够时，才进入Tier 2。V2改进了V1的摘要模板，搞了9段结构化格式：

```
## 1. 任务意图       — 用户想要达成什么
## 2. 技术概念       — 涉及的框架、库
## 3. 文件与代码     — 已创建/修改的文件路径及关键代码片段
## 4. 错误与修复     — 遇到的错误及解决方案
## 5. 方法           — 采用的实现路径
## 6. 用户消息       — 用户的关键指令和反馈
## 7. 待办           — 尚未完成的任务
## 8. 当前进展       — 已完成的步骤
## 9. 下一步         — 紧接着需要做什么
```

为什么要9段？因为之前的摘要模板太自由了，LLM经常丢关键信息。最致命的是丢**文件路径**和**错误信息**——Agent看到摘要后不知道之前改了哪些文件、遇到什么错误，就会重复犯错。

9段模板强制LLM把文件路径原文保留、错误信息原文保留、待办列出来。这样摘要之后，Agent不会"失忆"。

还有一个改进：Tier 2是基于Tier 1的结果做的。也就是说，LLM拿到的是微压缩后的历史，不是原始历史。输入更小，摘要LLM调用更便宜。

### 配置化

所有参数都在`agent.toml`里配：

```toml
[context]
max_output_tokens = 4096           # 预留输出token
compaction_threshold = 0.75        # Tier 2触发比例（占有效预算）
keep_recent_turns = 6              # 压缩时保留最近几轮
max_bash_output_chars = 20000      # run_bash输出截断
max_read_lines = 500               # read_file输出截断
microcompact_threshold = 20000     # Tier 1触发token阈值
microcompact_protected_results = 3 # Tier 1保护最近N个工具结果
```

有效预算 = 上下文窗口 - max_output_tokens。比如128K窗口、4096预留，有效预算123,904 tokens。Tier 2触发线 = 123,904 × 0.75 ≈ 92,928 tokens。

### 缓存

加了`compaction_cache`：当历史长度没变时，直接复用上次的压缩结果。Agent等用户输入时不会重复压缩。

---

## 六、代码结构

最终代码结构：

```
src/context.rs     — 核心逻辑
  ├── ContextConfig        配置结构体
  ├── TokenBudget          token预算追踪
  ├── estimate_tokens()    字符级token估算
  ├── estimate_history_tokens()  历史token估算
  ├── extract_text()       从Message提取纯文本
  ├── split_history()      切分新旧历史
  ├── microcompact()       Tier 1: 微压缩
  ├── format_messages_for_summary()  格式化给LLM
  ├── truncate_lines()     截断工具
  ├── truncate_at_char_boundary()  UTF-8安全截断
  └── COMPACTION_PREAMBLE  Tier 2摘要模板

src/agent_loop.rs   — Hook挂载
  ├── ContextHook          上下文管理Hook
  ├── handle_completion_call()  两层压缩入口
  └── compact_via_llm()    Tier 2 LLM调用
```

测试覆盖：

```
cargo test
running 103 tests
...
test context::tests::microcompact_clears_old_tool_results ... ok
test context::tests::microcompact_fewer_than_protected ... ok
test context::tests::microcompact_no_tool_results ... ok
test context::tests::microcompact_preserves_non_tool_content ... ok
...
test result: ok. 103 passed; 0 failed; 0 ignored
```

103个测试全过，0警告。

---

## 七、效果与成本

拿之前跑坦克大战的场景对比：

**V1（只有LLM摘要）：**
- 20轮跑下来，触发了4次压缩
- 每次压缩调一次摘要LLM，约2000 token输入 + 500 token输出
- 压缩总成本：4 × 2500 = 10,000 token

**V2（两层压缩）：**
- 同样20轮，Tier 1微压缩触发了6次
- 其中4次微压缩后就够低了，没调LLM
- 2次还是超了，进了Tier 2调LLM
- 压缩总成本：2 × 2500 = 5,000 token

**省了一半。**

如果用的是DeepSeek的缓存定价（标准输入的十分之一），微压缩省下的那4次LLM调用，几乎可以忽略不计。但省下的上下文token（每次微压缩清掉几千token的旧工具结果），让后续每一轮API调用的输入都更小，这个是持续的收益。

---

## 八、真实感受

做完这个上下文管理系统，有几个感受：

**1. 先调研再做，很重要。** 刚开始想自己拍脑袋想方案，后来花了一天看别人怎么做的，看完才知道每个方案的取舍在哪。Claude Code的五层压缩确实精细，但实现太复杂了，对moye这种个人项目来说，两层就够了。

**2. 最简单的方案往往最有效。** Tier 1微压缩的代码不到80行，逻辑就是"找到旧工具结果，替换成标记"。但效果最好——零成本砍掉30-50%的token。相比之下，Tier 2的LLM摘要要调API、要处理失败回退、要缓存，复杂得多，但只是Tier 1的补充。

**3. rig的PatchRequest设计得真好。** 每轮非粘性的历史替换，意味着Agent始终有完整的历史。压缩只是为了"发给API时省token"，不是真的删除记忆。这个设计让整个系统的风险很低——就算压缩出了问题，真实历史还在。

**4. 9段摘要模板比自由摘要好太多。** 之前LLM摘要经常丢信息，加了9段模板后，文件路径、错误信息、待办都强制保留了。Agent不再"失忆"。

**5. Token估算不需要精确。** 一开始纠结要不要装tokenizer，后来发现用字符级启发式+API返回的真实值校准，完全够了。上下文管理是"差不多就行"的问题——你不需要精确知道还剩多少token，只需要知道"快满了，该压了"。

当然，这个系统还有改进空间。比如Tier 1现在只看数量不看大小——一个巨大的工具结果（几千行代码）和一个很小的工具结果（"done"）同等对待。理想情况下应该按token大小来决定先清哪些。但这是优化，不是必须的。

项目代码都在GitHub上，欢迎查看：

https://github.com/spmno/moye

有什么问题或建议，评论区交流。
