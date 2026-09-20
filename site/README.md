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

### 1. 建项目（二选一）

**Git 集成（推荐，不需要任何密钥）**
Workers & Pages → Create → Pages → Connect to Git → 选本仓库，然后：

| 设置 | 值 |
| --- | --- |
| Framework preset | None |
| Build command | 留空 |
| Build output directory | `site` |

保存后每次推送自动部署，非生产分支会拿到预览域名。

**Direct Upload**

```sh
npx wrangler pages deploy site --project-name abylab
```

**CI 里推（可选）**：给仓库加两个 secret —— `CLOUDFLARE_API_TOKEN`
（权限 Pages: Edit）和 `CLOUDFLARE_ACCOUNT_ID`，`.github/workflows/site.yml`
就会在 `site/**` 有改动时用 wrangler 部署；没配 secret 时它自己跳过。
Git 集成已经在部署的话，把那个 workflow 删掉即可，别让两条路同时上线。

> 生产分支按仓库的实际流向选：站点改动在 `dev` 上写，随 `dev → main` 的发布
> 合并一起上线，所以 Pages 的 Production branch 填 `main`；`dev` 的推送会落到
> 预览域名，正好用来看排版。

### 2. 绑定域名

1. Pages 项目 → **Custom domains** → 添加 `abylab.ai`。域名 DNS 本来就在
   Cloudflare 的话，记录会自动建好，证书也会自动签发。
2. `www` 跳 apex（可选）：Cloudflare 官方做法是
   DNS 加一条 `A www → 192.0.2.1`（**Proxied**），再建一个
   **Bulk Redirect** 列表：`www.abylab.ai` → `https://abylab.ai`，状态 301，
   勾上 *Subpath matching* 和 *Preserve query string*。
   `_redirects` 里写不了带域名的整站跳转，所以这件事必须在 dashboard 做。
3. **SSL/TLS → Edge Certificates → Always Use HTTPS** 打开。

### 3. 检查

```sh
curl -sI https://abylab.ai/ | head -12        # CSP 等头部、200
curl -sI https://www.abylab.ai/ | head -3     # 301 → https://abylab.ai/
curl -s https://abylab.ai/install.sh | head -3   # 跟仓库里的脚本一致
```

## 改内容

- 文案只在两个 `index.html` 里，改中文别忘了 `en/index.html`；命令列表、按键、
  体积这些数字与 [README.md](../README.md) 保持一致，改一处记得改另一处。
- `styles.css` 只做 Pico 变量之外的一点点事；能靠 Pico classless 解决的样式就
  别往这儿加。
- 加了第三方脚本（统计、字体）记得同步放宽 `_headers` 里的 CSP。
- 升级 Pico：

  ```sh
  curl -fsSL -o site/vendor/pico.min.css \
    https://cdn.jsdelivr.net/npm/@picocss/pico@2.1.1/css/pico.min.css
  ```
