//! The dashboard's CSS. One const, inlined into every page's <head>.

pub const STYLE: &str = r#"
:root{
  --bg:#0d1017;--panel:#141a22;--panel-2:#1a212b;--raise:#1f2732;
  --line:#242d3a;--line-2:#323d4d;--ink:#e9edf3;--muted:#8a94a6;--faint:#5b6576;
  --accent:#7b8cff;--accent-2:#a97bff;--accent-ink:#0d1017;
  --run:#3fb950;--prov:#d8a123;--fail:#f8564b;
  --run-bg:rgba(63,185,80,.13);--prov-bg:rgba(216,161,35,.14);--fail-bg:rgba(248,86,75,.13);
  --radius:14px;--radius-sm:9px;
  --mono:ui-monospace,"SF Mono","JetBrains Mono",Menlo,Consolas,monospace;
  --sans:-apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,Inter,system-ui,sans-serif;
  --shadow:0 1px 2px rgba(0,0,0,.4),0 8px 24px -12px rgba(0,0,0,.55);
}
@media(prefers-color-scheme:light){:root{
  --bg:#f5f6f9;--panel:#fff;--panel-2:#f4f6f9;--raise:#fff;
  --line:#e6e9ef;--line-2:#d6dbe4;--ink:#161b24;--muted:#5b6675;--faint:#98a1b0;
  --accent:#5560e6;--accent-2:#7d4fe0;--accent-ink:#fff;
  --run:#1a7f37;--prov:#9a6700;--fail:#cf222e;
  --run-bg:rgba(26,127,55,.10);--prov-bg:rgba(154,103,0,.11);--fail-bg:rgba(207,34,46,.09);
  --shadow:0 1px 2px rgba(20,30,60,.06),0 12px 30px -18px rgba(20,30,60,.22);
}}
*{box-sizing:border-box}
body{margin:0;background:var(--bg);color:var(--ink);font-family:var(--sans);font-size:14px;line-height:1.5;
  -webkit-font-smoothing:antialiased;background-image:radial-gradient(1200px 500px at 80% -10%,rgba(123,140,255,.08),transparent 60%)}
a{color:var(--accent);text-decoration:none}a:hover{text-decoration:underline}
code{font-family:var(--mono)}h1,h2,h3{margin:0}button{font-family:inherit;cursor:pointer}
:focus-visible{outline:2px solid var(--accent);outline-offset:2px;border-radius:6px}
.topbar{position:sticky;top:0;z-index:10;display:flex;align-items:center;gap:1rem;
  padding:.85rem clamp(1rem,4vw,2.4rem);background:color-mix(in srgb,var(--bg) 82%,transparent);
  backdrop-filter:saturate(1.4) blur(10px);border-bottom:1px solid var(--line)}
.brand{display:flex;align-items:center;gap:.6rem}
.mark{width:26px;height:26px;flex:none}
.wordmark{font-weight:680;letter-spacing:-.01em;font-size:1.06rem}
.brand-sub{font-family:var(--mono);font-size:.66rem;letter-spacing:.16em;text-transform:uppercase;
  color:var(--muted);padding:.16rem .45rem;border:1px solid var(--line-2);border-radius:999px}
.top-actions{margin-left:auto;display:flex;align-items:center;gap:1rem}
.summary{color:var(--muted);font-size:.82rem}.summary b{color:var(--ink);font-weight:600}
.btn{font-size:.82rem;font-weight:560;border-radius:8px;padding:.5rem .85rem;border:1px solid var(--line-2);
  background:var(--panel-2);color:var(--ink);transition:.14s ease;line-height:1}
.btn:hover{border-color:var(--accent);transform:translateY(-1px)}
.btn.primary{border:0;color:var(--accent-ink);background:linear-gradient(135deg,var(--accent),var(--accent-2));box-shadow:0 6px 16px -8px var(--accent)}
.btn.primary:hover{filter:brightness(1.06)}.btn.ghost{background:transparent}
.btn.danger{background:transparent;border-color:transparent;color:var(--muted);padding:.4rem .55rem}
.btn.danger:hover{color:var(--fail);border-color:var(--fail);transform:none}
.icon-btn{background:transparent;border:1px solid transparent;color:var(--faint);width:26px;height:26px;
  border-radius:7px;display:grid;place-items:center;transition:.14s;font-size:.85rem}
.icon-btn:hover{color:var(--fail);border-color:var(--fail)}
.panel{background:var(--panel);border:1px solid var(--line);border-radius:var(--radius);box-shadow:var(--shadow);margin-top:1.4rem;overflow:hidden}
.panel-head{display:flex;align-items:baseline;gap:.8rem;flex-wrap:wrap;padding:1rem 1.2rem;
  border-bottom:1px solid var(--line);background:linear-gradient(180deg,var(--panel-2),transparent)}
.panel-title{display:flex;align-items:center;gap:.55rem}.proj-glyph{color:var(--accent);font-size:.9rem}
.panel-title h2{font-size:1.02rem;font-weight:640;letter-spacing:-.01em}
.panel-meta{color:var(--muted);font-size:.78rem;font-family:var(--mono)}
.panel-body{display:grid;grid-template-columns:1.55fr 1fr;gap:0}
.col{padding:1rem 1.2rem}
.col.environment{border-left:1px solid var(--line);background:color-mix(in srgb,var(--panel-2) 55%,transparent)}
.col.domain{grid-column:1/-1;border-top:1px solid var(--line)}
.col.registry{grid-column:1/-1;border-top:1px solid var(--line)}
.col-label{font-size:.68rem;letter-spacing:.15em;text-transform:uppercase;color:var(--muted);font-weight:600;
  margin-bottom:.7rem;display:flex;align-items:center;gap:.5rem}
.col-label .count{color:var(--faint);font-family:var(--mono);letter-spacing:0}
.deploy{position:relative;display:flex;gap:.7rem;align-items:flex-start;padding:.8rem .8rem .8rem .95rem;
  border:1px solid var(--line);border-left-width:3px;border-radius:var(--radius-sm);background:var(--raise);margin-bottom:.6rem;transition:.14s}
.deploy:hover{border-color:var(--line-2)}
.deploy.is-running{border-left-color:var(--run)}.deploy.is-provisioning{border-left-color:var(--prov)}.deploy.is-failed{border-left-color:var(--fail)}
.led{width:8px;height:8px;border-radius:50%;margin-top:.42rem;flex:none;background:var(--faint)}
.is-running .led{background:var(--run);animation:pulse 2.4s infinite}
.is-provisioning .led{background:var(--prov);animation:pulse 1.3s infinite}
.is-failed .led{background:var(--fail)}
@keyframes pulse{0%{box-shadow:0 0 0 0 color-mix(in srgb,var(--run) 60%,transparent)}70%{box-shadow:0 0 0 6px transparent}100%{box-shadow:0 0 0 0 transparent}}
.deploy-main{flex:1;min-width:0}
.deploy-row1{display:flex;align-items:center;gap:.6rem;flex-wrap:wrap}
.branch{font-family:var(--mono);font-weight:600;font-size:.9rem;letter-spacing:-.01em}
.pill{display:inline-flex;align-items:center;gap:.35rem;font-size:.7rem;font-weight:600;padding:.16rem .5rem;border-radius:999px}
.pill .dot{width:5px;height:5px;border-radius:50%;background:currentColor}
.pill.running{color:var(--run);background:var(--run-bg)}
.pill.provisioning{color:var(--prov);background:var(--prov-bg)}
.pill.failed{color:var(--fail);background:var(--fail-bg)}
.urls{display:flex;flex-wrap:wrap;gap:.35rem;margin-top:.5rem}
.chip{display:inline-flex;align-items:center;gap:.35rem;min-width:0;max-width:100%;font-family:var(--mono);
  font-size:.76rem;color:var(--ink);padding:.24rem .55rem;border:1px solid var(--line-2);border-radius:7px;background:var(--panel)}
.chip:hover{border-color:var(--accent);text-decoration:none;color:var(--accent)}
.chip svg{width:11px;height:11px;opacity:.6;flex:none}
.chip .host{overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.reason{margin-top:.5rem;font-size:.78rem;color:var(--fail);font-family:var(--mono)}
details.config{margin-top:.6rem}
details.config>summary{list-style:none;cursor:pointer;display:inline-flex;align-items:center;gap:.35rem;
  font-size:.75rem;color:var(--muted);font-weight:560;user-select:none}
details.config>summary::-webkit-details-marker{display:none}
.chev{transition:.15s;color:var(--faint)}details[open] .chev{transform:rotate(90deg)}
.svc-grid{display:grid;gap:.5rem;margin-top:.55rem}
.svc{border:1px solid var(--line);border-radius:8px;padding:.6rem .7rem;background:var(--panel-2)}
.svc-head{display:flex;align-items:center;gap:.5rem;margin-bottom:.35rem}
.svc-name{font-weight:620;font-size:.82rem}
.port{font-family:var(--mono);font-size:.68rem;color:var(--accent);background:color-mix(in srgb,var(--accent) 14%,transparent);padding:.06rem .38rem;border-radius:5px}
.svc .img{display:block;font-size:.75rem;color:var(--muted);word-break:break-all}
.env-inline{margin:.45rem 0 0;padding:0;list-style:none;display:grid;gap:.15rem}
.env-inline li{font-family:var(--mono);font-size:.73rem;color:var(--muted);word-break:break-all}
.env-inline .k{color:var(--ink)}.env-inline .eq{color:var(--faint)}
.env-list{display:grid;gap:.3rem;margin-bottom:.9rem}
.env-row{display:grid;grid-template-columns:1fr auto;align-items:center;gap:.3rem .6rem;
  padding:.5rem .6rem;border:1px solid var(--line);border-radius:8px;background:var(--raise)}
.env-row .k{font-family:var(--mono);font-size:.79rem;font-weight:600;color:var(--ink);word-break:break-all}
.env-row .val{font-family:var(--mono);color:var(--faint);letter-spacing:.05em;font-size:.8rem}
.env-meta{grid-column:1/2;display:flex;align-items:center;gap:.35rem;flex-wrap:wrap;margin-top:.15rem}
.env-row form{grid-row:1/3;grid-column:2;align-self:center}
.tag{font-family:var(--mono);font-size:.68rem;color:var(--muted);background:var(--panel-2);border:1px solid var(--line);padding:.05rem .4rem;border-radius:5px}
.tag.all{color:var(--accent);border-color:color-mix(in srgb,var(--accent) 35%,var(--line))}
.tag.default{color:var(--muted);border-style:dashed;border-color:var(--line-2)}
.tag.ok{color:var(--run);border-color:color-mix(in srgb,var(--run) 35%,var(--line));background:var(--run-bg)}
.tag.warn{color:var(--prov);border-color:color-mix(in srgb,var(--prov) 35%,var(--line));background:var(--prov-bg)}
.tag.bad{color:var(--fail);border-color:color-mix(in srgb,var(--fail) 35%,var(--line));background:var(--fail-bg)}
.env-meta .reason{margin-top:0;font-size:.73rem}
.retry{display:flex;align-items:center;gap:.6rem;flex-wrap:wrap;margin-top:.9rem}
.retry .hint{color:var(--muted);font-size:.78rem}
.add-var{display:grid;grid-template-columns:1fr;gap:.45rem;padding:.8rem;border:1px dashed var(--line-2);border-radius:9px}
.add-var input{width:100%;font-family:var(--mono);font-size:.8rem;color:var(--ink);background:var(--panel);
  border:1px solid var(--line-2);border-radius:7px;padding:.5rem .6rem}
.add-var input::placeholder{color:var(--faint)}
.add-var input:focus{outline:none;border-color:var(--accent)}
.empty{color:var(--muted);font-size:.82rem;padding:.9rem;border:1px dashed var(--line-2);border-radius:9px;text-align:center}
.login-wrap{min-height:100dvh;display:grid;place-items:center;padding:1.5rem}
.login-card{width:100%;max-width:360px;background:var(--panel);border:1px solid var(--line);border-radius:var(--radius);
  box-shadow:var(--shadow);padding:2rem 1.8rem;text-align:center}
.login-card .mark{width:40px;height:40px;margin:0 auto .8rem}
.login-card h1{font-size:1.25rem;letter-spacing:-.02em}
.login-card p{color:var(--muted);font-size:.85rem;margin:.35rem 0 1.4rem}
.login-form{display:grid;gap:.6rem}
.login-form input{font-size:.9rem;padding:.7rem .8rem;border-radius:9px;border:1px solid var(--line-2);
  background:var(--panel-2);color:var(--ink);text-align:center}
.login-form input:focus{outline:none;border-color:var(--accent)}
.login-form .btn.primary{padding:.7rem}
.err{color:var(--fail);font-size:.82rem;margin:0 0 .4rem}
@media(max-width:760px){.panel-body{grid-template-columns:1fr}
  .col.environment{border-left:0;border-top:1px solid var(--line)}}
@media(prefers-reduced-motion:reduce){*{animation:none!important;transition:none!important}}
/* --- app shell: sidebar + content (master-detail) --- */
.app{display:grid;grid-template-columns:248px 1fr;min-height:100dvh}
.rail{position:sticky;top:0;align-self:start;height:100dvh;display:flex;flex-direction:column;
  gap:.35rem;padding:1.1rem .8rem;border-right:1px solid var(--line);
  background:color-mix(in srgb,var(--panel) 55%,var(--bg))}
.rail .brand{display:flex;align-items:center;gap:.55rem;padding:.35rem .55rem 1rem}
.rail .wordmark{font-weight:680;letter-spacing:-.01em;font-size:1.02rem}
.nav-label{font-size:.66rem;letter-spacing:.16em;text-transform:uppercase;color:var(--faint);
  font-weight:600;padding:.9rem .6rem .35rem}
.nav-item{display:flex;align-items:center;gap:.55rem;padding:.5rem .6rem;border-radius:9px;
  color:var(--muted);font-size:.86rem;font-weight:540;transition:.13s}
.nav-item:hover{background:var(--panel-2);color:var(--ink);text-decoration:none}
.nav-item.active{background:color-mix(in srgb,var(--accent) 16%,transparent);color:var(--ink)}
.nav-item.active .glyph{color:var(--accent)}
.nav-item .glyph{color:var(--faint);font-size:.9rem;width:1rem;text-align:center}
.nav-spacer{flex:1}
.rail form{margin:0}
.content{min-width:0;width:100%;padding:clamp(1rem,3vw,2.2rem) clamp(1rem,4vw,2.6rem) 4rem;max-width:1100px}
.page-head{display:flex;align-items:baseline;gap:.8rem;flex-wrap:wrap;margin-bottom:.4rem}
.page-head h1{font-size:1.4rem;letter-spacing:-.02em;font-weight:660}
.page-sub{color:var(--muted);font-size:.85rem}
.stat-row{display:flex;gap:1.6rem;margin:1.2rem 0 .4rem}
.stat{display:flex;flex-direction:column;gap:.15rem}
.stat .n{font-size:1.5rem;font-weight:680;letter-spacing:-.02em}
.stat .l{font-size:.72rem;letter-spacing:.1em;text-transform:uppercase;color:var(--muted)}
/* --- live log panel --- */
details.logs{margin-top:.55rem}
details.logs>summary{list-style:none;cursor:pointer;display:inline-flex;align-items:center;gap:.35rem;
  font-size:.75rem;color:var(--muted);font-weight:560;user-select:none}
details.logs>summary::-webkit-details-marker{display:none}
.logterm{margin-top:.5rem;background:#0a0c11;border:1px solid var(--line-2);border-radius:9px;
  padding:.7rem .8rem;max-height:260px;overflow:auto;font-family:var(--mono);font-size:.74rem;
  line-height:1.5;color:#c8d0dc;white-space:pre-wrap;word-break:break-word}
.logterm .ph{color:var(--faint)}
@media(max-width:820px){.app{grid-template-columns:1fr}
  .rail{position:static;height:auto;flex-direction:row;flex-wrap:wrap;align-items:center;height:auto}
  .nav-spacer{display:none}}
"#;
