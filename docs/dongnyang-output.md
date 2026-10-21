# 출력 형식

한 줄에 JSON 하나. 모든 줄에 `kind`, `source`(플러그인 id), `outlet`, `fetched_at`(UTC), 대부분 `status`.

## 줄 종류

| kind | 언제 | 핵심 필드 |
|---|---|---|
| `feed_item` | RSS 항목마다 | 공통 필드(본문 없음), `feed`, `url` |
| `feed` | 피드마다 끝에 | `url`, `http_status`, `items`, `dropped`(링크·제목 없음), `encoding` |
| `sitemap_url` | 사이트맵의 기사 주소마다 | `id`, `url`, `lastmod`, `sitemap` |
| `sitemap_ref` | 사이트맵 색인의 하위 사이트맵마다 | `url`, `lastmod` |
| `sitemap` | 사이트맵마다 끝에 | `index`, `urls`, `old`(출처가 주지 않는 옛 기사 수) |
| `article` | 기사 페이지마다 | 공통 필드, `body`, `final_url`, `encoding`, `parser_version`(2) |
| `summary` | 실행 끝에 한 번 | `requests`, `statuses`(상태별 수), `stopped`, `unsent` |

## 공통 필드 (`--format normalized` 또는 `both`)

| 필드 | 형식 | 비고 |
|---|---|---|
| `id` | `"<source>:<기사 번호>"` | 플러그인 `id_pattern`으로 **요청한 주소**의 경로에서(쿼리 제외, 안 맞으면 canonical). 쿼리·`/amp/`가 달라도 같은 값. 못 뽑으면 `<source>:<추적 쿼리를 뺀 주소>` — 피드·사이트맵·기사 줄이 같은 값 |
| `canonical_url` | 주소 | canonical, 없으면 og:url. 추적 쿼리(SNS, utm_*, fbclid, gclid, 플러그인 `strip_params`) 제거 |
| `title`, `subtitle` | 문자열 | 앞뒤에 공백이 있는 구분자 + 언론사 이름 꼬리표(" \| 연합뉴스", " - 매일경제", 플러그인 `title_suffixes`, 긴 것부터) 제거. JSON-LD headline에는 적용하지 않음. "전망: 한국경제"처럼 붙은 명사는 남김 |
| `published_at`, `updated_at` | UTC ISO | 시각 없는 날짜는 쓰지 않음. 오프셋 없는 시각은 KST로 봄 |
| `authors` | 이름 목록 | 괄호 안("(서울=연합뉴스)", "(이메일)"), "지역=", " 기자"류, "글·사진" 같은 역할, 이름 옆 언론사 이름 제거. 주소·도메인·"뉴스"가 든 값·부서(…팀/부/센터)·한 글자·`author_ignore`는 버림 |
| `section_path` | 목록 | 계층 분류 중 가장 긴 것 |
| `categories` | 목록 | 모든 분류 출처의 값을 순서대로, 중복 제거 |
| `keywords` | 목록 | |
| `description` | 문자열 | 태그·엔티티 정리, 이메일 제거 |
| `image_url` | 주소 | 로고 이미지(`/logo/`, `_logo` 등)는 버림 |
| `provenance` | 필드 → 원본 키 | 값을 어디서 가져왔는지. 분류·키워드는 쓴 키들을 쉼표로 |
| `body` | 문단을 빈 줄로 구분 | 기사 줄에만. 이메일 제거, 최대 5만 자 |

## 원본 (`--format raw` 또는 `both`의 `raw`)

- 기사: `raw.page` = `meta`(속성 이름 소문자 → 값 목록, 페이지 순서), `jsonld`(객체 목록, `@graph`·배열 풀어서), `canonical`, `title`.
- RSS 항목: `raw.rss` = 직계 하위 요소 이름(접두어 포함: `dc:creator`, `media:content`) → 값 목록. 값은 텍스트, 또는
  속성이 있으면 `{"@속성": 값, "#text": 텍스트}`.
- 이메일 주소는 원본에서도 지운다.

## 매핑 규칙

필드마다 원본 키 후보를 순서대로 보고, 처음으로 쓸 만한 값을 주는 키가 이긴다. 기본 순서는 JSON-LD → meta → RSS.
플러그인 TOML의 `[map]`이 필드의 후보 목록을 통째로 바꾼다.

키: `meta:<이름>`, `jsonld:<점 경로>`(Article 계열 @type만, 예: `jsonld:author.name`), `jsonld:breadcrumb`,
`rss:<요소>[@속성]`, `canonical`, `title_tag`, `url:path`(주소의 앞쪽 소문자 경로 단계, 분류용),
`body:byline`(본문 끝의 "홍길동 기자" 줄, 기자용 마지막 후보).

분류(`[categories]`): `keys` 전부에서 값을 모아 `separators`(기본 `| > ›`, "IT/과학"을 지키려고 `/`는 빼 둠)로 나눈다. `hierarchical = true`(기본)이면
한 값이 경로(대분류|중분류)이고, JSON-LD 목록 하나도 경로 하나다. 버리는 값: 숫자뿐인 코드, 언론사 이름, 제목과 같은
값, 20자를 넘는 값(경로 끝의 제목), 홈·전체·뉴스·최신뉴스·기사, 플러그인 `ignore`. `url_depth`가 `url:path`의 단계 수.

키워드(`[keywords]`): `keys`(기본 `jsonld:keywords`, `meta:news_keywords`)를 `, ;`로 나눈다("한·미 정상회담"은 하나). 사이트 공통 값을 넣는
`meta:keywords`는 기본으로 읽지 않는다.

## 언론사별 특이사항 (2026-10-20 샘플 기준)

언론사를 더하거나 고칠 때 이 표에 한 줄을 남기고, 같은 구조를 흉내 낸 **직접 만든** HTML로 테스트를 단다.

| 플러그인 | 특이사항 | 처리 |
|---|---|---|
| `yna` 연합뉴스 | og:title 끝 " \| 연합뉴스", 경로 ["연합뉴스","최신뉴스"], 모바일 `/amp/view/`, RSS에 분류 없음, 피드 문구 "AI 학습 및 활용 금지" | 꼬리표 제거, 경로 잡음 버림, id 같음, 분류는 페이지 meta. 수집·판정 이용은 소유자 결정(2026-10-14) |
| `hankyung` 한국경제 | 2025-01 이전 기사 403, 1월 초 사이트맵에 2024-12 기사 섞임. 수집 IP 차단(2026-10-20) 뒤 페이지 샘플 없음 | `too_old`로 요청 안 함. 선택자는 이전 수집기 것 |
| `mk` 매일경제 | RSS pubDate `+09:00`, RSS author가 언론사, meta section은 영문 코드, 경로 끝이 제목 | 날짜 처리, 언론사 이름 버림, 분류는 JSON-LD·경로·RSS만 |
| `etnews` 전자신문 | JSON-LD 없음, meta가 `content` 먼저 쓰는 순서, meta keywords가 사이트 공통값, og:url에 `?SNS=` | 속성 순서 무관, keywords 안 읽음, 추적 쿼리 제거 |
| `chosunbiz` 조선비즈 | RSS·meta에 분류 없음(주소 경로에 있음), 광고 `.arcad-*`, 시각에 밀리초·Z | `url:path` 2단계, 광고 선택자 제외 |
| `fnnews` 파이낸셜뉴스 | 페이지에 기자·기사 이미지 없음(JSON-LD author·og:image가 언론사·로고), RSS에 있음, RSS 날짜 `Mon,19 Oct`, 분류 `경제\|일반경제` | `body:byline`, 로고 버림, 날짜 처리, 경로 |
| `etoday` 이투데이 | **RSS pubDate가 수정 시각**(게시 11:00, 피드 17:46), meta `dd:category` 숫자 코드, 피드 호스트 `rss.` | pubDate를 `updated_at`으로, 숫자 코드 버림, 두 호스트 |

## status

| status | 뜻 | 저장하는 쪽 |
|---|---|---|
| `ok` | 읽었고 본문이 있다 | 저장 |
| `no_body` | 읽었지만 본문 선택자가 빗나감(설명·meta는 있음) | 메타만 저장, 플러그인을 고친 뒤 다시 읽음 |
| `no_text` | 200 외의 상태(3xx 끝 등)로 쓸 내용 없음 | 기록 |
| `not_found` | 404·410 | 기록, 다시 읽지 않음 |
| `refused` | 403(5번 연속이면 차단 대기) | 다음에 다시, 횟수 제한 |
| `failed` | 응답 없음·429·5xx·끊긴 본문 | 다음에 다시, 횟수 제한 |
| `bad_encoding` | 깨진 글자가 1% 넘음 | 저장하지 않고 플러그인 확인 |
| `too_large` | 크기 한도 초과 | 기록 |
| `robots` / `redirect_blocked` | robots가 막음 / 리다이렉트가 규칙 밖 | 기록, 다시 읽지 않음 |
| `not_article` / `not_allowed` / `too_old` | 출처의 기사 주소가 아님 / 잘못된 주소 / 출처가 주지 않는 옛 기사. 요청 안 함 | 기록 |
| `busy` | `--max-wait`보다 오래 기다려야 해서 보내지 않음(사람이 기다리는 화면용) | 실패로 세지 않음, 잠시 뒤 다시 |
| `cooldown` / `state_unavailable` / `robots_unavailable` | 보내지 않았다. 이 실행은 멈춘다(종료 코드 4) | **실패로 세지 않고** 다음에 다시 |
| `not_modified` / `unparsable` / `no_sitemap` | 피드·사이트맵만: 바뀌지 않음 / 읽을 수 없음 / 그 기간 없음 | — |

## 저장하는 쪽이 지킬 것

- 열쇠는 피드·사이트맵이 준 `url` 그대로. `id`·`canonical_url`은 별도 열(같은 기사 묶기용).
- `published_at`이 없는 항목이 있다(이투데이 RSS). 페이지를 읽으면 채워진다.
- `status`가 `cooldown`, `state_unavailable`, `robots_unavailable`이면 그 주소는 다음에 다시 시도한다(실패로 세지 않는다).
- 종료 코드 4면 차단 대기나 상태 문제로 일찍 멈춘 것이다. `summary.unsent`만큼 보내지 않았다.
