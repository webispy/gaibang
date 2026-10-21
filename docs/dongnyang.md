# dongnyang (동냥)

API가 없는 웹사이트(언론사 RSS·사이트맵·기사 페이지)를 **한 번씩, 천천히** 청해서 읽고, 읽은 것마다 JSON 한 줄을
내보낸다. 출력 형식은 [dongnyang-output.md](dongnyang-output.md).

## 지키는 것 (옵션으로 끌 수 없음)

- 출처 플러그인의 호스트만 읽는다. 리다이렉트는 한 단계씩 같은 호스트·robots 허용 경로인지 확인한다.
- robots.txt(RFC 9309)를 User-Agent의 제품 토큰(`myresearch/1.0 …`이면 `myresearch`)으로 읽고, 하루 동안 공유 캐시한다
  (못 읽으면 1시간 동안 그 호스트를 쉰다). 브라우저인 척하는 User-Agent(`Mozilla/…`)는 받지 않는다.
- 간격은 `max(3초, 출처 최소 간격, Crawl-delay(60초까지), --interval)`, 최대 10분. 같은 `--state-dir`(절대 경로)을 쓰는 모든
  프로세스가 출처(또는 `pace_group`)마다 한 줄로 선다. 기다린 뒤 보내기 직전에 차단 대기를 다시 확인한다.
- 차단 신호면 그 출처는 모든 프로세스가 쉰다: Cloudflare 챌린지 24시간, 429는 Retry-After(없거나 날짜면 1시간),
  일반 403 5번 연속 24시간, 응답 없음·5xx·끊긴 본문 5번 연속 1시간. 같은 호스트를 읽는 출처는 같은 `pace_group`이어야
  한다(아니면 로드 오류). https에서 http로 가는 리다이렉트는 따르지 않는다.
- 상태 파일을 읽거나 쓰지 못하면(비었거나 깨져도) 아무것도 보내지 않는다. 상태는 잠금 아래 rename으로만 바뀐다.
  출력이 닫히면 바로 끝낸다(읽고 버리는 일 없음). 한 실행 안에서 같은 주소는 한 번만 읽는다.
- 출처가 제공하지 않는 옛 기사(주소의 날짜가 `first_day` 이전)는 요청하지 않는다.
- 출력의 이메일 주소는 지운다.

## 명령

```bash
S=$HOME/.local/state/gaibang                          # 절대 경로, 같은 기계의 모든 호출이 같은 곳
UA="myresearch/1.0 (+https://example.org/contact)"    # 누가 읽는지 밝힌다
dongnyang sources
dongnyang feed    --source yna --state-dir $S --user-agent "$UA"
dongnyang sitemap --source etoday --state-dir $S --user-agent "$UA" --from 2025-03-01 --to 2025-01-01
dongnyang article --source yna --state-dir $S --user-agent "$UA" --limit 50 < urls.txt
dongnyang ids < urls.txt                                # 기사 id만, 네트워크 없이
dongnyang parse --source yna --kind article --url https://www.yna.co.kr/view/AKR... page.html   # 네트워크 없이
```

공통 옵션: `--format normalized|raw|both`(기본 both), `--user-agent`(기본 `dongnyang/<버전> (+이 저장소)`),
`--header "Name: value"`(반복), `--interval 초`, `--timeout 초`, `--max-seconds 초`, `--max-wait 초`(순서를 그보다 오래
기다려야 하면 `busy`로 포기, 자리도 잡지 않음), `--sources DIR`(출처 TOML 추가·교체).

종료 코드: 0 끝까지, 2 잘못된 사용·설정, 3 출력 닫힘, 4 일찍 멈춤(차단 대기·상태 문제·시간). 마지막 줄은 `summary`.

## 에이전트에서 쓰기 (MCP)

`dongnyang mcp --state-dir <절대 경로>`는 stdio MCP 서버다. 도구: `list_sources`, `read_feed`(한 언론사의 최신 RSS
항목), `read_article`(기사 한 건의 공통 필드와 본문, 언론사 호스트의 주소만), `article_id`(읽지 않고 id만). 명령줄과 같은
규칙(robots.txt, 언론사마다 한 줄로 선 간격, 차단 대기)을 같은 상태 폴더로 지킨다. 순서를 `--max-wait`(기본 30초)보다
오래 기다려야 하면 `busy`로 답한다.

```bash
claude mcp add dongnyang -- dongnyang mcp --state-dir $HOME/.local/state/gaibang --user-agent "$UA"
```

## 출처 플러그인

`crates/dongnyang/sources/<id>.toml` 하나가 출처 하나(빌드에 포함, `--sources DIR`로 다시 빌드하지 않고 추가). 내장:
`yna` 연합뉴스, `hankyung` 한국경제, `mk` 매일경제, `etnews` 전자신문, `chosunbiz` 조선비즈, `fnnews` 파이낸셜뉴스,
`etoday` 이투데이. 마크업이 특이한 출처는 같은 명세 위에 `source::Plugin` 구현을 `code_plugin`에 등록한다.
언론사별 특이사항은 [출력 형식](dongnyang-output.md)의 표.

새 출처를 더하기 전에 그 언론사의 robots.txt와 이용약관이 자동 수집을 허용하는지 확인한다(AI·봇 차단 그룹이 있으면
넣지 않는다).

## 테스트

`cargo test`(workspace 전체)는 바깥 네트워크와 실제 키를 쓰지 않는다(로컬 서버, 가짜 transport만). 파서는 출처 마크업을 흉내 낸 **직접 만든** HTML로만
시험한다(기사 사본을 저장소에 넣지 않는다). 실제 사이트 점검은 출처당 몇 건만 한다.
