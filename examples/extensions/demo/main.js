// 示例扩展：面板 + 调 agent。
//
// 装法：把整个目录拷到 ~/.zex/extensions/demo，然后在「设置 → 扩展」里点打开。
//
// 两条规矩值得记住：
//
// 1. 宿主接口的模块名是 `zex.<扩展 id>`，不是固定的 `zex`。
//    因为模块注册表按名唯一，共用一个名字时后挂的会顶掉先挂的，
//    第一个扩展的 ask 就会发进第二个的会话。
//
// 2. render() 会被反复调用，别在里面做"每次都要发生一次"的事。
//    要跑一次的那种（初始化、只要问一次的问题）用一个 flag 挡住。
import { View, div } from "gpui-kit";
import { v_flex, h_flex, Button } from "gpui-base";
import { ask, name, reply, busy } from "zex.demo";

// 演示用：挂载后自动问一次，好让没手点的人也能看到整条链。
// 真做扩展时关掉它，改由按钮触发。
const AUTO_ASK = true;

export default class Demo extends View {
  init() {
    this.n = 0;
    this.autoAsked = false;
  }

  render(cx) {
    const c = cx.theme().colors;

    if (AUTO_ASK && !this.autoAsked) {
      this.autoAsked = true;
      // 不 await：回包由宿主投递并触发重绘，下一帧从 reply() 同步读。
      // 跨 await 的 cx 是失效的（gpui-shell 会明确报错），所以这条路更稳。
      ask("只回答四个字：链路已通").catch(() => {});
    }

    return v_flex()
      .size_full()
      .gap(10)
      .p(16)
      .bg(c.background)
      .child(div().text_lg().text_color(c.foreground).child(name()))
      .child(
        div()
          .text_sm()
          .text_color(c.muted_foreground)
          .child(`面板里的点击次数：${this.n}`),
      )
      .child(
        h_flex()
          .gap(8)
          .child(
            Button.new("inc")
              .h(30)
              .px(12)
              .bg(c.primary)
              .rounded(6)
              .on_click(() => {
                this.n += 1;
                cx.notify();
              })
              .child(div().text_color(c.primary_foreground).child("+1")),
          )
          .child(
            Button.new("ask")
              .h(30)
              .px(12)
              .bg(c.secondary)
              .rounded(6)
              .on_click(() => ask("再说一句话").catch(() => {}))
              .child(div().text_color(c.secondary_foreground).child("问 agent")),
          ),
      )
      .child(
        div()
          .text_sm()
          .text_color(c.muted_foreground)
          // busy() 由主线程查"这个会话有没有人在等回包"
          .child(busy() ? "等待回复…" : reply() || "（还没问过）"),
      );
  }
}
