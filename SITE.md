# abylab.ai

网站源码：纯静态，没有构建步骤，没有 npm 依赖。HTML + 一个样式表 + 一个很小的
脚本，CSS 框架是自托管的 [Pico CSS](https://picocss.com/)（classless 用法）。
整站零外部请求，托管在 Cloudflare Pages。

```
site/
  index.html            中文首页（默认）
  en/index.html         英文首页
  404.html              Pages 的 404 页（自动生效，返回 404 状态码）
  styles.css            Pico 之上的薄薄一层：header、hero、安装块、footer
  app.js                主题切换 + 复制按钮（全部前端逻辑）
  install.sh            安装脚本，仓库根目录那份的逐字节副本
  vendor/pico.min.css   Pico v2.1.1，自托管，不用 CDN
  favicon.svg · robots.txt · sitemap.xml
  _headers              Pages 响应头：安全头 + CSP + 缓存
  _redirects            目前没有规则，只留说明（见下）
```

`_headers` 和 `_redirects` 只在 Cloudflare Pages 上生效（由 Pages 解析，不会被
当成静态文件发给浏览器）。本地用 `python3 -m http.server` 预览时它们只是两个
普通文件，看不到效果。

`install.sh` 有两份：仓库根的（打包进 tarball、`gh` 拉取用）和 `site/` 的（公网
提供 `https://abylab.ai/install.sh`）。以前是 `_redirects` 里一条 302 跳回
raw.githubusercontent.com，网络到不了 GitHub 的人就装不了；现在站点直接发这份
文件，`_redirects` 里也**不能**再写 install.sh 规则——跳转会盖过同路径的静态
文件。两份靠 CI 里的一条 `diff` 检查保持一致，改一份不改另一份会挂。

`site/` 里的每个文件都会原样发布到公网——所以这份说明放在仓库根目录（和
[README.md](README.md)、[TECH.md](TECH.md) 做邻居），不跟站点一起上线。

## 本地预览

```sh
npx wrangler pages dev site     # 完整行为，含 _headers / _redirects
# 或者只求能看：
python3 -m http.server 8000 --directory site
```

路径都是站点根相对（`/styles.css`），要用服务器预览，别直接双击打开文件。

## 部署到 Cloudflare Pages

现状：**已经上线**。<https://abylab.ai> 与 <https://abylab.pages.dev> 都在服务
`site/` 的内容（与本地逐字节一致，只有 `/index.html` 被 308 规范到 `/`）。
项目 `abylab` 的 Production branch = `main`，Pages 对 `abylab.ai` 的域名校验
已 active（Let's Encrypt 证书，SAN `abylab.ai` + `*.abylab.ai`）。

### 0. 域名上的两个坑（都踩过了）

**apex 必须 CNAME 到 `abylab.pages.dev`**，不能是一条指向 `192.0.2.1` 之类的 A
记录。代理开着、记录存在，看起来一切正常，但边缘连不到源站，整站返回 **522**，
同时 Pages 那边一直显示 `CNAME record not set`。另外在 dashboard 点
"Add custom domain" 时 Cloudflare 会顺手建记录，而用 API 挂域名时**不会**。

**zone 级 Web Analytics 会往 HTML 里塞 beacon**。abylab.ai 走的是 zone，所以
Cloudflare 会对浏览器请求注入一段 `static.cloudflareinsights.com/beacon.min.js`
（带 `data-cf-beacon` 和 SRI）。我们的 CSP 只有 `script-src 'self' 'unsafe-inline'`，
于是这段脚本被拦（`requestfailed: csp`，`transferSize: 0`）——没有真的发出请求，
但**每次加载都报一条 CSP 错误**，而统计也什么都收不到。二选一：

- 想要"零第三方 + 干净控制台"：Analytics & Logs → Web Analytics 关掉该站点。
- 想留统计：在 `_headers` 的 CSP 里放开来源，然后重新部署：

  ```
  script-src 'self' 'unsafe-inline' https://static.cloudflareinsights.com
  connect-src https://cloudflareinsights.com
  ```

`www` 目前还没解析。要做 `www → apex` 的 301：DNS 加 `A www → 192.0.2.1`
（Proxied）**并且**建 Bulk Redirect（`www.abylab.ai` → `https://abylab.ai`，301，
勾 Subpath matching + Preserve query string）——只有记录没有跳转，www 会 522。

### 1. 部署

**Direct Upload（现在用的这条）**

```sh
npx wrangler pages deploy site --project-name abylab --branch main
```

`--branch main` 是必须的：本地在 `dev` 上开发，而 `main` 是项目的生产分支，随便
推一个分支只会得到预览部署，自定义域名不会跟着更新。工作区有未提交改动时
wrangler 会拦一下，加 `--commit-dirty=true` 跳过。

**Git 集成（想改成推仓库自动部署）**
Workers & Pages → Create → Pages → Connect to Git → 选本仓库，然后：

| 设置 | 值 |
| --- | --- |
| Framework preset | None |
| Build command | 留空 |
| Build output directory | `site` |

保存后每次推送自动部署，非生产分支会拿到预览域名；`dev` 的推送正好用来看排版。

**CI 里推（可选）**：给仓库加两个 secret —— `CLOUDFLARE_API_TOKEN`
（权限 Pages: Edit）和 `CLOUDFLARE_ACCOUNT_ID`，`.github/workflows/site.yml`
就会在 `site/**` 有改动时用 wrangler 部署；没配 secret 时它自己跳过。

### 2. 绑定域名（已完成，备份步骤）

1. Pages 项目 → **Custom domains** → 添加 `abylab.ai`（已添加，校验 active）。
2. DNS → Records 加一条 apex 记录：

   | Type | Name | Target | Proxy |
   | --- | --- | --- | --- |
   | CNAME | `@`（即 `abylab.ai`） | `abylab.pages.dev` | **Proxied** |

   apex 上用 CNAME 是可以的，Cloudflare 会做 flattening。记录一出现，Pages
   那边 `CNAME record not set` 的校验就通过并自动签发证书。
3. **SSL/TLS → Edge Certificates → Always Use HTTPS** 打开（`http://abylab.ai`
   现在 301 到 https）。

### 3. 检查

```sh
curl -sI https://abylab.ai/ | head -12            # 200 + CSP 等头部
curl -sI http://abylab.ai/ | head -3              # 301 → https://abylab.ai/
curl -sI https://abylab.ai/install.sh | head -1   # 200，不是 302（跳回 GitHub 就说明 _redirects 被写回去了）
curl -sI https://abylab.ai/nope | head -1         # 404，且渲染我们的 404 页
for f in index.html styles.css app.js vendor/pico.min.css install.sh; do
  diff <(curl -s https://abylab.ai/$f) site/$f && echo "$f 一致"
done
```

### 4. 安装包镜像（abylab.ai/downloads）

`release.yml` 的 `mirror` job 在发布后把四个平台的 tarball 复制到站点上，给
GitHub Releases 慢或被墙的网络留一条路：

```
/downloads/latest/VERSION                              v0.1.2
/downloads/latest/abylab-latest-<target>.tar.gz        ×4
/downloads/latest/SHA256SUMS                           对应上面的固定文件名
```

三个必须记住的点：

- **一次推整站**：Pages 的部署是上传目录的完整快照，所以 job 先把 `site/`
  复制进 staging，再放 `downloads/`。只推 `downloads/` 会把整站覆盖掉，
  `_headers`、`_redirects` 也会一起消失。
- **site/ 取默认分支，不取 tag**：站点不跟着版本走，所以 mirror job 的 checkout
  用的是默认分支。回填旧 tag（`-f tag=v0.1.1`）时如果跟着 tag 取 `site/`，推上
  去的就是那个 tag 当时的站点——没有 `site/install.sh`、`_redirects` 里还带着
  跳 GitHub 的规则，正好把 #7 修掉的东西盖回去。
- **必须 `--branch main`**：tag 推送时 checkout 的不是分支，wrangler 会当成
  预览部署，自定义域名不会更新。
- **只留最新版**：快照语义决定老的 `downloads/v0.1.1/` 在下一次发布时就没了，
  所以镜像只有 `latest`（job 里也会比对最新 release，避免回填旧 tag 时把新版
  覆盖掉）。指定 `--version v0.1.1` 的安装仍然走 GitHub Releases。

需要 `CLOUDFLARE_API_TOKEN`（Pages: Edit）和 `CLOUDFLARE_ACCOUNT_ID` 两个
secret；没配时 job 跳过，并在 run summary 里写出补配方法。配好之后回填某次发布：

```sh
gh workflow run release.yml -f tag=v0.1.2
```

`install.sh` 对 `latest` 会**先试镜像再回退 GitHub**（镜像 404/超时/缺文件都
不阻塞安装）：`--no-mirror` 或 `ABYLAB_MIRROR=''` 只走 GitHub，
`--mirror <url>` 换一个镜像地址。检查：

```sh
curl -s https://abylab.ai/downloads/latest/VERSION
curl -sO https://abylab.ai/downloads/latest/abylab-latest-x86_64-unknown-linux-gnu.tar.gz
curl -sO https://abylab.ai/downloads/latest/SHA256SUMS && sha256sum -c --ignore-missing SHA256SUMS
```

## 改内容

- 文案只在两个 `index.html` 里，改中文别忘了 `en/index.html`；命令列表、按键、
  体积这些数字与 [README.md](README.md) 保持一致，改一处记得改另一处。
- `styles.css` 只做 Pico 变量之外的一点点事；能靠 Pico classless 解决的样式就
  别往这儿加。
- 加了第三方脚本（统计、字体）记得同步放宽 `_headers` 里的 CSP。
- 改了 `install.sh` 记得 `cp install.sh site/install.sh`（CI 会 diff 两份，忘了
  会直接挂）。

## 自托管的 Pico

页面只引 `/vendor/pico.min.css`，没有任何 CDN、webfont 或第三方请求：整站唯一
的 origin 就是站点自己。这份文件就是官方 npm 包里的那一份，逐字节一致：

```
版本    Pico CSS v2.1.1（MIT，版权声明写在文件头的注释里）
sha256  fbc9a63fc9fc9f72d12fd7fc9806e11fa9f77ae4f9cad146b27003a1119ba3db
大小    83,319 字节
```

它内部没有 `@import`，所有 `url()` 都是内嵌的 `data:image/svg+xml`（自带的表单
控件图标），所以自托管之后不会再往外发请求。升级时从 npm registry 取包（不经
CDN），并核对 `css/pico.min.css` 的哈希：

```sh
ver=2.1.1
curl -fsSL -o /tmp/pico.tgz "https://registry.npmjs.org/@picocss/pico/-/pico-$ver.tgz"
tar xzOf /tmp/pico.tgz package/css/pico.min.css > site/vendor/pico.min.css
sha256sum site/vendor/pico.min.css      # 与上面那行比对，然后更新这里的版本号
```

版本号和哈希同时写在上面这个块里和本文件开头的目录清单里，升级后两处都要改。

想确认线上确实没有任何外链，抓一下页面里的静态资源引用就够了——它们必须全
是本站路径（`canonical` / `alternate` 是给搜索引擎看的声明，不是会去请求的
资源，所以排除掉）：

```sh
curl -s https://abylab.ai/ \
  | grep -Eo '<(link|script)[^>]*(href|src)="[^"]*"' \
  | grep -vE 'rel="(canonical|alternate)"' | grep -v '"[/]' \
  || echo "no external assets"
```
