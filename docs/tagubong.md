# tagubong (타구봉)

공식 API를 내 키로 부르고, 답 하나를 JSON 한 줄로 내보낸다. DB를 모른다.

## 지키는 것

- 키는 **환경 변수로만** 받는다(명령줄 인자는 프로세스 목록에 보인다). 키·요청 주소(키가 쿼리에 들어가는 API가 있다)·
  응답 원문 오류는 출력·오류 메시지에 남기지 않는다. 키 모양이 아니면(쿼리를 바꿀 문자) 실행하지 않는다.
- API마다 간격을 둔다. 같은 `--state-dir`을 쓰는 모든 프로세스가 API마다 한 줄로 선다(`api-<이름>` 키, dongnyang과 같은 상태 폴더).
- 429면 Retry-After(없으면 10분) 동안 그 API는 모든 프로세스가 쉰다. 응답 없음·5xx면 그 실행을 멈추고, 5번 연속이면
  1시간 쉰다. 401·403·키 거절이면 멈춘다(재시도해도 같은 답).
- `--endpoint`(테스트용, 숨김)는 https, 또는 loopback의 http만: 키가 평문으로 나가지 않게.

종료 코드: 0 끝까지, 2 잘못된 사용·키 없음, 3 출력 닫힘, 4 일찍 멈춤(키 거절, 429, 응답 없음, 차단 대기, 상태 문제, 시간).
마지막 줄은 `summary`(`requests`, `statuses`, `stopped`, `unsent`).

## API

| API | 키 | 명령 | 간격 |
|---|---|---|---|
| `fred` 미국 세인트루이스 연준 FRED | `FRED_API_KEY` | `observations --series ID… --start YYYY-MM-DD` | 0.6초 (허용 분당 120) |
| `kis` 한국투자증권 Open API | `KIS_APP_KEY`, `KIS_APP_SECRET` | `kis get`: 요청을 stdin에 한 줄씩 | 0.2초 |
| `toss` 토스증권 Open API | `TOSS_CLIENT_ID`, `TOSS_CLIENT_SECRET` | `toss get`: 요청을 stdin에 한 줄씩 | 요청 그룹마다(초당 3~20건) |
| `ecos` 한국은행 ECOS | `ECOS_API_KEY` (경로의 `{key}`) | `get --api ecos` | 0.5초 |
| `datagokr` 공공데이터포털 | `DATA_GO_KR_KEY` (`serviceKey`) | `get --api datagokr` | 1초 |
| `assembly` 열린국회정보 | `ASSEMBLY_API_KEY` (`KEY`) | `get --api assembly` | 1초 |
| `law` 법제처 국가법령정보 | `LAW_OC` (`OC`) | `get --api law` | 1초 |
| `krx` KRX Open API | `KRX_API_KEY` (헤더 `AUTH_KEY`) | `get --api krx` | 0.3초, 401·403은 그 서비스만의 거절(서비스마다 승인) |

`tagubong apis`가 이 표를 JSON 줄로 낸다.

OpenDART는 없다: `opendart.fss.or.kr`는 TLS 1.2의 RSA 키 교환 암호(AES128-GCM-SHA256 등)만 받는데(2026-10-21 확인),
tagubong이 쓰는 rustls는 그 암호를 쓰지 않아 연결이 되지 않는다.

## 출력

### `fred observations`

시리즈마다 한 줄:

| 필드 | 내용 |
|---|---|
| `kind`, `api`, `series`, `start` | `"observations"`, `"fred"`, 요청한 값 |
| `status` | `ok`, `error`(그 시리즈만의 오류: 없는 id 등), 또는 실행을 멈춘 이유(`key_rejected`, `rate_limited`, `unavailable`, `cooldown`, `state_unavailable`: 뒤의 시리즈는 보내지 않음) |
| `http_status` | 보낸 경우 |
| `observations` | `ok`일 때 `[{"date", "value"}]`. 값이 없는 날(휴일)은 `"."` |
| `error` | `error`일 때 `error_code N`만(FRED 메시지는 남기지 않음) |
| `fetched_at` | 답을 받은 뒤의 UTC 시각 |

### `kis get`

stdin 한 줄이 요청 하나: `{"id": 아무 값(그대로 돌려줌), "path": "/uapi/…", "tr_id": "…", "params": {"이름": "값"}, "tr_cont": ""}`.
`path`는 `/uapi/`로 시작하는 경로만, `tr_id`·`tr_cont`·파라미터 이름은 영숫자만 받는다(주소·헤더를 바꿀 수 없게). 값은
퍼센트 인코딩한다. 답은 요청마다 한 줄:

| 필드 | 내용 |
|---|---|
| `kind`, `api`, `id`, `tr_id` | `"response"`, `"kis"`, 요청의 값 |
| `status` | `ok`(`rt_cd` "0"), `error`(KIS의 오류: `msg_cd`, `error` = `"<tr_id>: <msg_cd> <msg1>"`, 본문이 JSON이 아니면 `"<path>: HTTP <상태>"`), `bad_request`(요청 줄이 틀림), 또는 멈춘 이유(`key_rejected`, `token_unavailable`, `rate_limited`, `unavailable`, `cooldown`, `state_unavailable`) |
| `payload` | `ok`일 때 KIS 응답 본문 그대로(키·시크릿·토큰 문자열은 `[key]`로) |
| `tr_cont` | `ok`일 때 응답 헤더의 연속 조회 값(`M`·`F`면 다음 쪽이 있음: 같은 요청을 `tr_cont: "N"`으로) |

- 접근 토큰은 상태 폴더 `credentials/kis.json`(0600)에 두고 모든 프로세스가 같이 쓴다. 만료 30분 전까지 쓰고, 그 뒤나
  "토큰 무효·만료"(EGW00121·EGW00123) 답이면 한 번 새로 받아 다시 묻는다. 발급은 잠금 아래 한 프로세스만 한다.
- "초당 거래건수 초과"(EGW00201)면 1초 쉬고 한 번 더, 응답 없음이면 2초 쉬고 한 번 더. KIS는 오류를 5xx와 본문으로
  알리므로 본문이 있는 5xx는 KIS의 답으로 읽는다(본문 없는 5xx만 실패로 센다).
- 마지막 `summary`에 `token_issued`(이 실행이 새로 받은 토큰 수).

### `toss get`

stdin 한 줄이 요청 하나: `{"id": 아무 값, "path": "/api/v1/…", "params": {"이름": "값"}, "group": "STOCK"}`. `group`은
토스의 호출 한도 묶음(`MARKET_DATA_CHART`, `MARKET_INDICATOR_CHART`, `MARKET_INFO`, `STOCK`, `SECTOR`)이고 묶음마다 따로
간격을 둔다(상태 폴더의 `api-toss-<묶음>`). 답은 요청마다 한 줄: `status`가 `ok`면 `payload`에 응답 본문 그대로(자격
증명 문자열은 `[key]`), `error`면 `code`(토스의 오류 코드)와 `request_id`만.

- 토스는 클라이언트마다 토큰 하나만 유효하다(새로 받으면 앞의 것이 바로 무효). 그래서 토큰은 상태 폴더
  `credentials/toss.json`(0600)에 두고 모든 프로세스가 같이 쓴다. 만료 5분 전까지 쓰고, "만료·취소·무효"
  (`expired-token`·`token-revoked`·`invalid-token`) 답이면 한 번 새로 받아 다시 묻는다.
- 429면 Retry-After만큼(없으면 2·4·8초, 60초까지) 쉬고, GET의 5xx면 1·2·4초 뒤 다시, 각각 3번까지.
- 토큰 발급이 401·403이면 멈춘다(`key_rejected`). 데이터 요청의 401·403은 그 요청의 오류다(토스는 종목 하나를 `not-found`로 거절하기도 한다).

### `get --api <이름>` (키를 붙인 GET)

stdin 한 줄이 요청 하나: `{"id": 아무 값, "path": "<기본 주소 아래 경로>", "params": {"이름": "값"}, "accept": "…"}`.
경로 조각은 영숫자와 `-_.`만(`..`·빈 조각 없음), 키는 API마다 정한 자리(쿼리 이름, 경로의 `{key}` 조각, 헤더)에
tagubong이 넣는다. 요청이 키 자리를 쓰려 하면 받지 않는다. 답은 요청마다 한 줄:

| 필드 | 내용 |
|---|---|
| `status` | `ok`(2xx), `error`(그 밖의 상태: `error` = `"<path>: HTTP <상태>"`), `too_large`(32 MB 넘음), `bad_request`, 또는 멈춘 이유 |
| `http_status`, `content_type` | 보낸 경우 |
| `body` | 본문이 UTF-8이면 텍스트(키는 `[key]`로), 아니면 `body_base64`(zip 등) |

API마다 오류를 본문에 적는 방식이 달라(결과 코드, XML 메시지) 본문 해석은 호출하는 쪽이 한다.

## API 더하기

키를 붙인 GET이면 `keyed::SPECS`에 한 줄(기본 주소, 키 자리, 간격, 거절의 뜻). 토큰 발급 같은 절차가 있으면 `crates/tagubong/src/<api>.rs` 하나에 엔드포인트, 키 환경 변수, 간격, 응답을 줄로 바꾸는 함수를 둔다. 요청은
`caller::Caller`로만 보낸다(간격·차단 대기·멈춤을 거기서 지킨다). 테스트는 가짜 transport와 로컬 서버로만 한다.
