Phase is an open source Magic: The Gathering project.

All game interactions and cards should follow the Magic comprehensive rules set. The comprehensive rules may be found in this same directory at COMPRULES.txt.

#Card Oracle Text
The Oracle text of cards provides the latest, official rules for individual cards. Use the Scryfall API to get Oracle Text of cards. The API is described here: https://scryfall.com/docs/api

Card Search API Example:
curl -s -G "https://api.scryfall.com/cards/named" \
  --data-urlencode "exact=Delver of Secrets" \
  -H "User-Agent: MyApp/1.0" \
  -H "Accept: application/json;q=0.9,*/*;q=0.8" \
  | jq -r '.oracle_text // ([.card_faces[].oracle_text] | join("\n---\n"))'

#Additive Instructions
When interactions are found which may apply to more than one card or more than one condition, make a note of that interacton in the file INTERACTIONS.txt such that it could be applied to future cards with the same interaction.

Examples
1) Izzet Cronarch can return an instant or sorcery card to a player's hand. Archaeomancer can also return an instant or sorcery card to a player's hand. These cards should operate in the same way.
2) Psychatog gets +1/+1 until end of turn from discarding a card. Wild Mongrel can also gain +1/+1 until end of turn from discarding a card.



#Contributing
Every change you make is an AI-assisted contribution to phase-rs/phase. Follow the repository's docs/AI-CONTRIBUTOR.md from start to finish, including:
- Identify the Gate B anchors (at least two existing analogous implementations) before writing code.
- Implement through the $engine-implementer workflow, including its PR preparation handoff, which syncs your branch with upstream origin/main after implementing.
- On the final committed head, run the final read-only review and then Gate A (./scripts/check-parser-combinators.sh).
- Write the pull request draft from .github/PULL_REQUEST_TEMPLATE.md, filling every section from these actual results, including the exact Gate A and Final review-impl lines. Never fill in a result that was not produced.

If a gate fails, or the work falls outside what AI-CONTRIBUTOR.md allows, stop and explain why in your summary instead of writing the pull request draft. Do not push or open the pull request yourself; that is done for you once you finish.
