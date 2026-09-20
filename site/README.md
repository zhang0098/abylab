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
  vendor/pico.min.css   Pico v2.1.1，自托管，不用 CDN
  favicon.svg · robots.txt · sitemap.xml
  _headers              Pages 响应头：安全头 + CSP + 缓存
  _redirects            Pages 跳转：/install.sh → 仓库里的安装脚本
```

`_headers` 和 `_redirects` 只在 Cloudflare Pages 上生效（由 Pages 解析，不会被
当成静态文件发给浏览器）。本地用 `python3 -m http.server` 预览时它们只是两个
普通文件，看不到效果。

## 本地预览

```sh
npx wrangler pages dev site     # 完整行为，含 _headers / _redirects
# 或者只求能看：
python3 -m http.server 8000 --directory site
```

路径都是站点根相对（`/styles.css`），要用服务器预览，别直接双击打开文件。

## 部署到 Cloudflare Pages

现状：项目 `abylab` 已经建好（Production branch = `main`），站点已经部署上去，
生产地址 <https://abylab.pages.dev> 就是 `site/` 的内容（逐字节一致）。自定义
域名 `abylab.ai` 已挂到项目上，卡在 **等待一条 DNS 记录**——见下面第 2 步。

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

### 2. 绑定域名

1. 自定义域名已经添加。剩下一条 DNS 记录：**DNS → Records → 添加**

   | Type | Name | Target | Proxy |
   | --- | --- | --- | --- |
   | CNAME | `@`（即 `abylab.ai`） | `abylab.pages.dev` | **Proxied** |

   apex 上用 CNAME 是可以的，Cloudflare 会做 flattening。记录一出现，Pages
   那边的校验（`CNAME record not set`）就会通过并自动签发证书，不用再做别的。

   注意：在 dashboard 里点 "Add custom domain" 时 Cloudflare 会顺手把这条记录
   建好；通过 API 挂域名时它**不会**——所以要么在面板里加，要么用带
   *Zone → DNS → Edit* 权限的 token 调
   `POST /zones/{zone_id}/dns_records`。

2. `www` 跳 apex（可选）：Cloudflare 官方做法是
   DNS 加一条 `A www → 192.0.2.1`（**Proxied**），再建一个
   **Bulk Redirect** 列表：`www.abylab.ai` → `https://abylab.ai`，状态 301，
   勾上 *Subpath matching* 和 *Preserve query string*。
   `_redirects` 里写不了带域名的整站跳转，所以这件事必须在 dashboard 做。
3. **SSL/TLS → Edge Certificates → Always Use HTTPS** 打开。

### 3. 检查

```sh
curl -sI https://abylab.pages.dev/ | head -12     # CSP 等头部、200
curl -sI https://abylab.ai/ | head -3             # 域名生效后：200
curl -sI https://www.abylab.ai/ | head -3         # 301 → https://abylab.ai/
curl -s https://abylab.ai/install.sh | head -3    # 跟仓库里的脚本一致
```

## 改内容

- 文案只在两个 `index.html` 里，改中文别忘了 `en/index.html`；命令列表、按键、
  体积这些数字与 [README.md](../README.md) 保持一致，改一处记得改另一处。
- `styles.css` 只做 Pico 变量之外的一点点事；能靠 Pico classless 解决的样式就
  别往这儿加。
- 加了第三方脚本（统计、字体）记得同步放宽 `_headers` 里的 CSP。

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
