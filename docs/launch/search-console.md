# Search verification — human click instructions

No accounts or DNS records were created or changed. Use existing authorized Google, Microsoft and Cloudflare access. These steps document TXT verification only; do not modify MX, SPF, DKIM, DMARC, nameservers, CNAMEs or existing TXT values.

## Google Search Console

1. Open [Search Console](https://search.google.com/search-console) in an existing account. Choose Add property → Domain → `toanother.one` (no scheme/path).
2. Choose TXT in the domain-verification dialog and copy the exact generated verification value. Do not invent one.
3. In Cloudflare, select the `toanother.one` zone → DNS → Records → Add record. Type TXT, Name `@`, Content exactly the generated `google-site-verification=...` value, TTL Auto. Add a separate record; do not replace an existing TXT record. Save only after the human checks the zone, name, type and value.
4. Return to Search Console and click Verify. If DNS has not propagated, wait and retry without deleting working mail records. Keep the verification TXT after success.
5. In Sitemaps submit `https://toanother.one/sitemap.xml`. Use URL inspection for the home page and each new page, inspect Google's selected canonical and request indexing where appropriate. A submission is not a promise of indexing.

[Google's verification instructions](https://support.google.com/webmasters/answer/9008080?hl=en) checked 2026-11-05.

## Bing Webmaster Tools

Bing's documented manual DNS method is CNAME, not a generic TXT token. Do not fabricate a Bing TXT record. To remain within the TXT-only boundary:

1. First complete Google's TXT verification above.
2. Open [Bing Webmaster Tools](https://www.bing.com/webmasters/) in an existing authorized account.
3. Choose Import from Google Search Console if available. The human must review the requested Google authorization and select only the verified `toanother.one` property. Do not grant unexpected permissions or create an account.
4. Confirm the imported property and sitemap. If not imported, add `https://toanother.one/sitemap.xml` through the Sitemaps UI.
5. If import is unavailable or requires unwanted access, stop. The human must choose a separately authorized non-DNS verification method; this packet does not authorize CNAME/Domain Connect changes.

[Bing add/verify guidance](https://www.bing.com/webmasters/help/add-and-verify-site-12184f8b) was consulted 2026-11-05; its live help UI requires JavaScript. Confirm the current import flow in the account. This is a documented decision branch, not a claim that Bing verification was executed.

## GitHub social preview

The supplied `site/og.png` is 1200×630. A human can open the repository's Settings → General → Social preview → Edit → Upload an image. GitHub's standard gh repo edit command does not expose a social-preview upload option. No upload or metadata reset was performed in this lane.
