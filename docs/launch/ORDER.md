# Launch order — recommendation, not a schedule

No posts have been submitted or scheduled. Timing below is a recommendation; it does not assume an existing audience or predict traffic.

1. Finish independent site review and deploy the validated output. Verify production URLs, deep 404, metadata and social images. Have a human inspect the first-run instructions in a fresh environment.
2. Prepare the v0.1.0 release separately. A maintainer must approve/tag it; validate archive installs and checksums on the target platforms before announcing binary availability. Resolve current hardening lanes and CI before calling the release ready. This launch lane does not certify those changes.
3. Using existing accounts, verify Search Console and import into Bing. Submit the sitemap and inspect canonical URLs. Indexing is not a launch prerequisite and is not guaranteed.
4. Consider Show HN first only after v0.1.0 is tagged, installation works and a human maker writes original submission/comment text under HN's rules. The model-written comment has been removed. Link the runnable repository and choose a day with maker response coverage; do not solicit engagement.
5. After responding to the first feedback, adapt the r/mcp draft. Use r/ClaudeCode's current weekly showcase thread for a short, disclosed project share. Space these across different days and tailor each post; do not mass cross-post. Skip r/LocalLLaMA's model-written draft: its rules prohibit primarily LLM-generated copy.
6. Consider the disclosed Hashnode tutorial after a human has run and approved it. DEV's AI-assisted-content rules prohibit promotional articles, so its file contains human preparation notes only. Do not automatically cross-post to DEV or HN. Share to an eligible Discord showcase channel only after checking its rules. Lobsters requires human authorship, an account older than its first 70 days, a self-promotion share below a quarter over the last year and a substantive computing angle.
7. Product Hunt belongs on a separate day after the initial fixes and a real demo capture are ready. Its guidance requires personal maker accounts and disallows company accounts. Defer if that cannot satisfy the no-real-name boundary; no account was created.
8. Submit to appropriate awesome lists only after release availability and their contribution rules are satisfied; there is no authorization here to open those PRs. An own-brand X account is recommended for future use, but creation and posting remain human decisions.

Measure useful outcomes without adding site tracking: reproducible install reports, actionable issues, documentation questions and voluntary feedback. GitHub stars and rankings are not acceptance evidence.

## Decisions before public launch

- Approve the release/tag and confirm install results on macOS and Linux.
- Approve channel copy and choose which existing accounts can represent Another without exposing a personal name.
- HN and DEV require the human-authorship/eligibility decisions in their preparation files; model-written promotional text is not ready for those channels.
- Product Hunt: defer unless a policy-compliant, privacy-compatible maker identity already exists.
- r/LocalLLaMA: skip, or have a human independently write a relevant local-model resource-sharing post under current rules; do not paste this draft.
- Optional: record a real terminal demo. No GIF or video is claimed in this packet.
