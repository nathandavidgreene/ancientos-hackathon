# Backlog (from Nathan's feedback on the first live brief, 2026-10-26)

Core scope stays locked until the Oct 30 gate; these are ordered candidates after that.

1. **Media in briefs**: show images/video from the post (X `attachments.media_keys` + `media.fields=url,preview_image_url,type`).
2. **"Try it" tag**: hands-on posts (stretching, physio, meditation, breathing). Brief shows numbered steps + media, then a follow-up next day: "Did you try it?" (yes / not yet / not for me), logged as feedback.
3. **"Go deeper" later**: queue a pick for a guided walkthrough at a scheduled time (scheduler job), not in the daily brief itself.
4. **Delegate inspiration**: `delegate` verdict hands a post to the agent, which unrolls the author's thread and writes notes to a private inspiration file (built).
5. **Stretch: custom X feed ranking**: roll accumulated verdicts (approve / skip / never_post / never_category / delegate) into a ranking model for a personal X feed.

## Idea cards (inspiration capture) — approved by Nathan 2026-10-26, after media-in-briefs
- Delegate verdict creates an idea card: source link, idea, AncientOS mapping (e.g. gate, decision_log), effort, status=new.
- Cards appended to an append-only log like decision_log.
- Weekly one-at-a-time triage: adopt / park / drop. Adopt -> backlog item for Dev; park resurfaces in 30 days.
- Demo beat: bookmark -> idea -> roadmap.
- New component name: idea_card (tell Dev for hero-names map).
