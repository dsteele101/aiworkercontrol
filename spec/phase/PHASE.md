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


