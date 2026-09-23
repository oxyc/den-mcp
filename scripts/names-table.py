#!/usr/bin/env python3
"""Writes src/names_table.rs: countries and languages by name, from Wikidata.

    python3 scripts/names-table.py > src/names_table.rs

Countries are the items with a current ISO 3166-1 alpha-2 code (P297), and the Q-id is what `citizenship` takes.
Languages are the items with an ISO 639-1 code (P218). The aliases below are how people name them in a question:
demonyms and short names, ported from den-atlas's `facets.rs` COUNTRIES table, plus the ones that table leaves out
because in free text they usually mean something else ("english", "georgia", "cuba") — here the kind says which.
"""

import json
import sys
import unicodedata
import urllib.parse
import urllib.request

SPARQL = "https://query.wikidata.org/sparql"
AGENT = "den-mcp-names-table/1 (https://github.com/oxyc/den-mcp)"

# Codes Wikidata gives two current items for, where the one atlas's people carry is not the oldest: Danish and
# Dutch people are recorded as citizens of the kingdoms. Any other such code takes its oldest (lowest) item, the
# country's own.
PREFER = {"DK": "Q756617", "NL": "Q29999"}
# The name a person would say, where Wikidata's label is the formal state's.
DISPLAY = {"DK": "Denmark", "NL": "Netherlands", "CN": "China", "BS": "Bahamas"}
# Codes a corpus title can still carry for a state that no longer exists.
HISTORICAL = {
    "DD": ("Q16957", "East Germany"),
    "SU": ("Q15180", "Soviet Union"),
    "CS": ("Q33946", "Czechoslovakia"),
    "YU": ("Q36704", "Yugoslavia"),
}

COUNTRY_ALIASES = """
american america usa US; united states US; british britain uk GB; great britain GB; united kingdom GB;
england english scottish scotland welsh wales GB; french france FR; east german DD; east germany DD;
german germany DE; italian italy IT; canadian canada CA; spanish spain ES; australian australia AU;
north korean KP; north korea KP; south korean KR; south korea KR; korean korea KR; indian india bollywood IN;
japanese japan JP; belgian belgium BE; mexican mexico MX; chinese china CN; brazilian brazil BR;
argentine argentinian argentina AR; hong kong HK; hongkong HK; swedish sweden SE; danish denmark DK;
russian russia RU; polish poland PL; swiss switzerland CH; austrian austria AT; dutch netherlands holland NL;
turkish turkey TR; czech czechia CZ; czech republic CZ; finnish finland FI; norwegian norway NO; irish ireland IE;
colombian colombia CO; new zealand NZ; new zealander NZ; chilean chile CL; greek greece GR; thai thailand TH;
hungarian hungary HU; taiwanese taiwan TW; romanian romania RO; indonesian indonesia ID; icelandic iceland IS;
iranian iran IR; peruvian peru PE; portuguese portugal PT; israeli israel IL; bulgarian bulgaria BG;
south african ZA; south africa ZA; venezuelan venezuela VE; emirati uae AE; filipino philippines PH;
egyptian egypt EG; cuban cuba CU; ukrainian ukraine UA; nigerian nigeria nollywood NG; serbian serbia RS;
estonian estonia EE; cypriot cyprus CY; algerian algeria DZ; uruguayan uruguay UY; luxembourgish luxembourg LU;
dominican DO; dominican republic DO; afghan afghanistan AF; belarusian belarus BY; bolivian bolivia BO;
croatian croatia HR; lithuanian lithuania LT; bosnian bosnia BA; yugoslav yugoslavian yugoslavia YU;
soviet ussr SU; soviet union SU; czechoslovak czechoslovakian czechoslovakia CS;
pakistani pakistan PK; albanian albania AL; vietnamese vietnam VN; jamaican jamaica JM;
burkinabe BF; burkina faso BF; malaysian malaysia MY; palestinian palestine PS; lebanese lebanon LB;
jordanian jordan JO; azerbaijani azerbaijan AZ; botswana BW; ecuadorian ecuador EC; armenian armenia AM;
congolese congo CD; kazakh kazakhstan KZ; sri lankan LK; sri lanka LK; ivorian CI; ivory coast CI;
senegalese senegal SN; paraguayan paraguay PY; singaporean singapore SG; bangladeshi bangladesh BD;
cambodian cambodia KH; maltese malta MT; latvian latvia LV; iraqi iraq IQ; bhutanese bhutan BT;
moroccan morocco MA; libyan libya LY; bahamian bahamas BS; burmese burma myanmar MM; guatemalan guatemala GT;
ethiopian ethiopia ET; montenegrin montenegro ME; ugandan uganda UG; panamanian panama PA; kenyan kenya KE;
kyrgyz kyrgyzstan KG; macedonian macedonia MK; north macedonia MK; mozambican mozambique MZ;
mauritian mauritius MU; rwandan rwanda RW; andorran andorra AD; sudanese sudan SD; ghanaian ghana GH;
cameroonian cameroon CM; costa rican CR; costa rica CR; gambian gambia GM; guinean guinea GN; haitian haiti HT;
liechtenstein LI; monegasque monaco MC; malian mali ML; mongolian mongolia MN; mauritanian mauritania MR;
nepali nepalese nepal NP; papua papuan PG; puerto rican PR; puerto rico PR; qatari qatar QA; saudi SA;
saudi arabia SA; slovenian slovene slovenia SI; slovak slovakia SK; syrian syria SY; chadian chad TD;
tajik tajikistan TJ; tunisian tunisia TN; vatican VA; vanuatu VU; kosovar kosovo XK; zambian zambia ZM;
georgian georgia GE; zimbabwean zimbabwe ZW; tanzanian tanzania TZ; angolan angola AO; macanese macau MO
"""

LANGUAGE_ALIASES = """
farsi persian fa; mandarin cantonese zh; flemish nl; tagalog filipino tl; bokmal bokmål norwegian no;
gaelic scottish gaelic gd; irish gaelic ga; sami northern sami se; serbo croatian sh; hebrew he;
castilian es; brazilian portuguese pt; swiss german de; greenlandic kl; faroese fo; frisian fy
"""


def query(text):
    url = SPARQL + "?" + urllib.parse.urlencode({"query": text})
    req = urllib.request.Request(url, headers={"Accept": "application/sparql-results+json", "User-Agent": AGENT})
    with urllib.request.urlopen(req, timeout=120) as resp:
        return json.load(resp)["results"]["bindings"]


def qid(binding):
    return binding["item"]["value"].rsplit("/", 1)[1]


def fold(text):
    text = unicodedata.normalize("NFD", text.lower())
    text = "".join(c for c in text if not unicodedata.combining(c))
    return " ".join("".join(c if c.isalnum() else " " for c in text).split())


def aliases(block):
    """`word word CODE; …` → [(folded alias, CODE)], each word (or the whole run before the code) an alias."""
    out = []
    for entry in block.replace("\n", " ").split(";"):
        words = entry.split()
        if not words:
            continue
        code, names = words[-1], words[:-1]
        phrase = " ".join(names)
        out.append((fold(phrase), code))
        out.extend((fold(w), code) for w in names)
    return out


def rust(s):
    return json.dumps(s, ensure_ascii=False)


def main():
    countries = {}
    rows = query(
        "SELECT ?code ?item ?itemLabel WHERE { ?item p:P297 ?st . ?st ps:P297 ?code . "
        "FILTER NOT EXISTS { ?st pq:P582 ?end } "
        'SERVICE wikibase:label { bd:serviceParam wikibase:language "en". } }'
    )
    for b in rows:
        code, item, label = b["code"]["value"], qid(b), b["itemLabel"]["value"]
        if code in PREFER and item != PREFER[code]:
            continue
        if code in countries and int(countries[code][0][1:]) < int(item[1:]):
            continue
        countries[code] = (item, label)
    countries.update(HISTORICAL)
    country_aliases_from_labels = {fold(label): code for code, (_, label) in countries.items()}
    for code, name in DISPLAY.items():
        countries[code] = (countries[code][0], name)

    languages = {}
    for b in query(
        'SELECT ?code ?item ?itemLabel WHERE { ?item wdt:P218 ?code . '
        'SERVICE wikibase:label { bd:serviceParam wikibase:language "en". } }'
    ):
        code, label = b["code"]["value"], b["itemLabel"]["value"]
        if code in languages and languages[code] != label:
            sys.exit(f"{code}: two languages, {languages[code]} and {label}")
        languages[code] = label.capitalize() if label.islower() else label

    country_aliases = dict(aliases(COUNTRY_ALIASES))
    for alias, code in country_aliases_from_labels.items():
        country_aliases.setdefault(alias, code)
    for code, (_, label) in countries.items():
        country_aliases.setdefault(fold(label), code)
    language_aliases = dict(aliases(LANGUAGE_ALIASES))
    for code, label in languages.items():
        language_aliases.setdefault(fold(label), code)
    for alias, code in list(country_aliases.items()):
        if code not in countries:
            sys.exit(f"alias {alias!r} names {code}, which Wikidata has no current item for")

    print("// Generated by scripts/names-table.py from Wikidata; edit the script, not this file.")
    print("// Countries (ISO 3166-1 alpha-2, P297) with their Wikidata item, and languages (ISO 639-1, P218).")
    print()
    print("/// Code, English name, and the Wikidata item a person's citizenship names.")
    print("pub const COUNTRIES: &[(&str, &str, &str)] = &[")
    for code in sorted(countries):
        item, label = countries[code]
        print(f"    ({rust(code)}, {rust(label)}, {rust(item)}),")
    print("];")
    print()
    print("/// A country's name or demonym, folded (`names::fold`), and its code.")
    print("pub const COUNTRY_ALIASES: &[(&str, &str)] = &[")
    for alias in sorted(country_aliases):
        print(f"    ({rust(alias)}, {rust(country_aliases[alias])}),")
    print("];")
    print()
    print("/// Code and English name.")
    print("pub const LANGUAGES: &[(&str, &str)] = &[")
    for code in sorted(languages):
        print(f"    ({rust(code)}, {rust(languages[code])}),")
    print("];")
    print()
    print("/// A language's name or another name for it, folded, and its code.")
    print("pub const LANGUAGE_ALIASES: &[(&str, &str)] = &[")
    for alias in sorted(language_aliases):
        print(f"    ({rust(alias)}, {rust(language_aliases[alias])}),")
    print("];")


if __name__ == "__main__":
    main()
