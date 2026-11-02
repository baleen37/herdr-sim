# herdr-radar에서 참고할 로컬 상태 개선

조사일: 2026-11-02. 비교 대상은 radar main의
`54a30926c43b2867ddb7bbb7d167492751e3c43a`와 herdr-sim `43e763c`에
현재 작업 중인 세션 누락 시 제목 삭제 변경을 더한 상태다.
Radar 소스와 회귀 테스트를 읽었으며 설치하거나 UI를 실행하지 않았다.
아래 적용 방향은 코드 비교에 따른 제안이며 구현·효과 검증 결과가 아니다.

## 이미 반영된 기능

workspace/worktree 트리, 활동순 정렬, 확인할 때까지 done 유지,
재작업까지 blocked 유지, idle의 15분·2시간 구분, spinner와 pulse는
이미 herdr-sim에 있다. 새로 참고할 핵심은 유지한 상태를 언제 풀고
잘못된 표시를 어떻게 복구하는가다.

## 개선 후보

| 순서 | Radar 구현 | herdr-sim 적용 방향 | 주의점 |
| --- | --- | --- | --- |
| 1 | 시작 시 확인 창 이후 idle과 정식 lifecycle 출처의 idle에서는 blocked 유지 해제 | `states()`와 `held_status()`에 필요한 근거를 전달해 예외 추가 | Codex는 현재 화면 감지 기반이므로 lifecycle 예외만으로 현재 사례가 해결되지는 않음 |
| 2 | pane ID 변경을 `terminal_id`로 추적하고 기존 상태 이관 | 이동 시 held/heading과 전송 캐시를 정리하고 새 pane에 재보고 | 프로세스가 상속한 오래된 `HERDR_PANE_ID`는 별도 본체·연동 문제 |
| 3 | working 직후 짧은 idle/done을 2.5초 흡수 | 마지막 working 시각과 만료 후 재확인을 함께 구현 | 완료 표시가 늦어짐. 타이머 없이 조건만 추가하면 working 고착 가능 |
| 4 | 1초 heartbeat와 재연결 후 전체 재동기화 | 낮은 빈도의 상태 전용 재동기화부터 검토 | git/gh 전체 갱신을 매초 실행하지 않기 |
| 5 | 세션 파일 누락 시 현재 터미널·metadata 제목 사용 | 이전 캐시 대신 live API 제목을 정제해 fallback으로 표시 | `Action Required`, spinner, 경로 등 상태·장식 문구를 작업 제목과 구분 |

### 1. blocked 해제 조건

[frame.js 471–505](https://github.com/hhdebb/herdr-radar/blob/54a30926c43b2867ddb7bbb7d167492751e3c43a/lib/frame.js#L471-L505)는
처음 작업하기 전 시작 확인 상태와 `screen_detection_skipped=true`인
정식 lifecycle 상태를 구분한다.
[회귀 테스트](https://github.com/hhdebb/herdr-radar/blob/54a30926c43b2867ddb7bbb7d167492751e3c43a/test/blocked-authoritative-idle.test.js#L81-L120)도 있다.
현재 sim의 `src/lib.rs`의 `held_status()`는 idle이면 이전 blocked를
무조건 유지하며, `src/main.rs`의 `states()`는 상태 출처를 읽지 않는다.

### 2. 이동 후 동일성

[frame.js 644–674](https://github.com/hhdebb/herdr-radar/blob/54a30926c43b2867ddb7bbb7d167492751e3c43a/lib/frame.js#L644-L674)는
동일 terminal의 새 pane ID를 찾아 상태를 이관한다.
[이동 테스트](https://github.com/hhdebb/herdr-radar/blob/54a30926c43b2867ddb7bbb7d167492751e3c43a/test/pane-move.test.js#L71-L125)가 있다.
Sim의 Cache는 heading/held/depth를 pane ID로 저장한다.
표시 상태의 이관과 호출 위치 복구는 서로 다른 문제로 검증해야 한다.

### 3. 짧은 오판 흡수

[frame.js 449–469](https://github.com/hhdebb/herdr-radar/blob/54a30926c43b2867ddb7bbb7d167492751e3c43a/lib/frame.js#L449-L469)와
[기본값](https://github.com/hhdebb/herdr-radar/blob/54a30926c43b2867ddb7bbb7d167492751e3c43a/lib/config.js#L178-L188)을
참고할 수 있다. blocked는 즉시 표시하되 짧은 idle/done 변동만 흡수한다.

### 4. 이벤트 누락 복구

[heartbeat](https://github.com/hhdebb/herdr-radar/blob/54a30926c43b2867ddb7bbb7d167492751e3c43a/lib/daemon.js#L356-L360),
[재연결](https://github.com/hhdebb/herdr-radar/blob/54a30926c43b2867ddb7bbb7d167492751e3c43a/lib/daemon.js#L301-L311),
[캐시 무효화](https://github.com/hhdebb/herdr-radar/blob/54a30926c43b2867ddb7bbb7d167492751e3c43a/lib/frame.js#L120-L147)를
함께 봐야 한다. Sim은 실행 시 최신 목록을 읽고 startup에서 전송 캐시를
비우지만, 이벤트 사이의 animator는 기존 토큰만 움직이며 상태를 재판정하지 않는다.

### 5. 현재 제목 fallback

[state.js 310–334](https://github.com/hhdebb/herdr-radar/blob/54a30926c43b2867ddb7bbb7d167492751e3c43a/lib/state.js#L310-L334)는
현재 live 제목을 정제한다. 이전 제목을 지우는 현재 수정 다음에 검토할 수 있다.

## 권장 다음 검증

시작 확인 창에서 blocked가 된 뒤 idle로 돌아오는 시나리오를 먼저 재현한다.
그 다음 물음표 해제 예외 하나만 추가한다. 현재 Codex의 오래된
`Action Required` 제목과 pane ID 불일치는 별도 재현으로 유지한다.
