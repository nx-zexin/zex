// 示例扩展：一屏看完"扩展能做什么"。
//
// 装法：把整个目录拷到 ~/.zex/extensions/demo，然后在「设置 → 扩展」里点打开。
//
// 这一屏是个自检面板：每行一个能力，右边是**此刻的真实返回值**。
// 想确认哪一环断了，看哪一行是空的就行。
//
// 三条规矩值得记住：
//
// 1. 宿主接口的模块名是 `zex.<扩展 id>`，不是固定的 `zex`。
//    模块注册表按名唯一，共用一个名字时后挂的会顶掉先挂的，
//    第一个扩展的 ask 就会发进第二个的会话。
//
// 2. render() 会被反复调用，别在里面做"每次都要发生一次"的事。
//    要跑一次的用 flag 挡住（下面 autoAsked 就是）。
//
// 3. cx 只属于产出它的那一次调用。回调里要用回调自己的参数
//    （on_click((_event, cx) => ...)），跨 await 的 cx 同样失效 ——
//    所以拿回包推荐"不 await、下一帧读 reply()"。
import { View, div } from "gpui-kit";
import { v_flex, h_flex, Button } from "gpui-base";
import { ask, name, reply, busy, session } from "zex.demo";

// 挂载后自动问一次：无头环境点不到按钮，靠它让 agent 那条链跑起来。
// 真做扩展时关掉，改由按钮触发。
const AUTO_ASK = true;

export default class Demo extends View {
  init() {
    // 纯脚本本地状态，宿主完全不知道它的存在
    this.n = 0;
    this.autoAsked = false;
    // 上一次 ask 的失败原因（成功时为空）
    this.lastError = "";
  }

  // 一问一答。不 await：回包由宿主投递并触发重绘，
  // 下一帧从 reply() 同步读 —— 这条路不碰失效的 cx。
  fire(prompt) {
    this.lastError = "";
    ask(prompt).catch((e) => {
      this.lastError = String(e);
    });
  }

  render(cx) {
    const c = cx.theme().colors;

    if (AUTO_ASK && !this.autoAsked) {
      this.autoAsked = true;
      this.fire("只回答四个字：链路已通");
    }

    const waiting = busy();
    const lastReply = reply();

    // 一行自检项：能力名 + 真实值
    const check = (label, value, ok) =>
      h_flex()
        .gap(8)
        .items_center()
        .child(
          div()
            .w(14)
            .text_sm()
            .text_color(ok ? c.primary : c.muted_foreground)
            .child(ok ? "✓" : "·"),
        )
        .child(div().w(120).text_sm().text_color(c.foreground).child(label))
        .child(
          div()
            .text_sm()
            .text_color(c.muted_foreground)
            .child(value),
        );

    return v_flex()
      .size_full()
      .gap(8)
      .p(16)
      .bg(c.background)
      // ─── 身份：两个宿主函数 ───
      .child(div().text_lg().text_color(c.foreground).child(name()))
      .child(
        div()
          .text_xs()
          .text_color(c.muted_foreground)
          .child(`会话 ${session()}`),
      )
      // ─── 自检清单 ───
      .child(
        div()
          .mt(4)
          .text_xs()
          .text_color(c.muted_foreground)
          .child("自检（右边是此刻的真实值）"),
      )
      .child(check("脚本渲染", "这一屏就是", true))
      .child(check("本地状态", `${this.n} 次`, this.n > 0))
      .child(check("宿主 name()", name(), true))
      .child(check("宿主 busy()", waiting ? "等待回包" : "空闲", !waiting))
      .child(check("agent 回包", lastReply || "（还没回）", Boolean(lastReply)))
      // ─── 两个按钮 ───
      .child(
        h_flex()
          .gap(8)
          .mt(6)
          .child(
            Button.new("inc")
              .h(30)
              .px(12)
              .bg(c.primary)
              .rounded(6)
              // 注意回调的第二个参数：那才是"这次点击"的 cx。
              // 用 render 时抓到的 cx 会报 "cx is no longer valid"
              // —— 它只属于产出这一帧的那次调用。
              .on_click((_event, clickCx) => {
                this.n += 1;
                clickCx.notify();
              })
              .child(
                div()
                  .text_color(c.primary_foreground)
                  .child("本地 +1（不碰宿主）"),
              ),
          )
          .child(
            Button.new("ask")
              .h(30)
              .px(12)
              .bg(c.secondary)
              .rounded(6)
              .on_click(() => this.fire("再说一句话"))
              .child(
                div()
                  .text_color(c.secondary_foreground)
                  .child("问 agent（走内核）"),
              ),
          ),
      )
      // ─── 状态行 ───
      .child(
        div()
          .mt(4)
          .text_xs()
          .text_color(this.lastError ? c.danger : c.muted_foreground)
          .child(
            this.lastError
              ? `上一次失败：${this.lastError}`
              : waiting
                ? "等待回包…"
                : lastReply
                  ? `最后回复：${lastReply}`
                  : "（按上面的按钮）",
          ),
      );
  }
}
