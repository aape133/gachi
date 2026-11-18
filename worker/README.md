# gachi 연결 서버 (Cloudflare Worker)

방에 **들어갈 때만** 쓰는 작은 서버예요. 오디오·채팅·멤버 목록은 여기를 거치지 않아요.

| 경로 | 하는 일 |
|---|---|
| `/ws/host` | 방장이 입장 코드를 열어 둠 (코드가 켜져 있는 동안만 연결 유지) |
| `/ws/join/{코드}` | 새 사람의 입장 요청을 방장에게 전달하고, 방장의 답을 돌려줌 |
| `/ws/nearby?lan=…` | 같은 네트워크(공인 주소 + 사설 /24)에 열린 방 목록 (방장이 "보여주기"를 켠 방만) |
| `/j/{코드}` | 초대 링크 페이지 (앱을 `gachi://join/{코드}`로 엶) |
| `/health` | `ok` |

- 방 코드마다 Durable Object 하나(`Jam`)가 방장 연결 하나만 붙잡고 있어요. 메시지가 없을 때는 잠들어 있어서 비용이 들지 않아요. 방을 만들 때 처음 저장하고, 방이 끝나면 저장한 것을 모두 지워요.
- Cloudflare에 남는 것: 방 코드, 코드를 다시 열 때 쓰는 열쇠(`host_key`), 최대 인원, 현재 인원. 방장의 봉인용 공개 키, 비밀번호가 있는지 여부. 입장 요청과 답은 봉인된 채로 지나가서 Cloudflare는 내용을 읽지 못하고, **비밀번호는 아예 오지 않아요**.
- 방장 연결이 끊기면 코드를 30초 동안 지켜줘요. 그 사이에 방장(또는 다음 방장)이 열쇠로 다시 가져가요. 아무도 가져가지 않으면 방을 잊어요.
- 코드 맞히기 시도는 IP당 1분에 20번으로 제한해요(`Counts`).
- 같은 네트워크의 방 목록은 네트워크(공인 IPv4, IPv6는 /64)마다 `Local` Durable Object 하나에 있어요. 방 이름·방장 닉네임·인원·비밀번호 여부만 담고, 코드를 끄거나 방이 사라지면 지워요.

## 처음 배포하기

1. [Cloudflare](https://dash.cloudflare.com/sign-up)에 가입하고 이메일 인증을 해요 (무료 플랜).
2. 이 폴더에서 로그인해요. 브라우저가 열리면 로그인하고 **Allow**를 누르세요.

   ```bash
   npx wrangler login
   ```

3. 배포해요.

   ```bash
   npx wrangler deploy
   ```

   `wrangler.toml`의 `routes` 줄(`gachi.band`)은 공개된 gachi의 도메인이라 지우고 배포해요. 내 도메인을 쓰려면 그 자리에 내 도메인을 적어요(Cloudflare에 등록된 도메인이어야 해요).

   끝나면 `https://gachi.<계정이름>.workers.dev` 같은 주소가 나와요.

4. 그 주소를 앱의 기본 서버로 넣고 앱을 다시 빌드해요: `app/src/settings.rs`의 `DEFAULT_SERVER`.
   공개된 gachi는 `gachi.band`를 써요.

처음 배포할 때 업로드 직후 "This Worker does not exist on your account [code: 10007]"가 날 수 있어요.
업로드는 된 상태라서 `npx wrangler deploy`를 한 번 더 하면 끝까지 진행돼요.

코드를 고친 뒤에는 `npx wrangler deploy`만 다시 하면 돼요. 로그는 Cloudflare 대시보드의 Workers 화면이나 `npx wrangler tail`로 볼 수 있어요.

## 내 PC에서 실행·테스트 (계정 필요 없음)

처음 한 번 `npm install`을 해요.

```bash
npm run dev
```

```bash
npm test
```

`npm run dev`는 `127.0.0.1:8787`에서 Worker를 띄워요. 앱을 `GACHI_SERVER=127.0.0.1:8787`로 실행하면 배포된 서버 대신 이걸 써요.
`npm test`는 Worker를 직접 띄워서 방 열기·입장·거절·코드 끄기·이어받기·시도 제한을 실제 WebSocket으로 확인해요.

이 서버도 gachi의 일부로 GPLv3예요([LICENSE](../LICENSE)).

wrangler가 Cloudflare에 보내는 익명 사용 통계를 끄려면 환경 변수 `WRANGLER_SEND_METRICS=false`를 쓰세요.

## 무료 플랜 한도

- Durable Objects: 하루 요청 10만 건, 실행 시간 13,000 GB-s. 대기 중(hibernation)인 방은 실행 시간이 과금되지 않아요.
- 앱은 20초마다 `ping`을 보내는데, 서버가 방을 깨우지 않고 자동으로 `pong`을 돌려줘서 한도에 거의 영향이 없어요.
- 실제 사용량은 Cloudflare 대시보드에서 확인할 수 있어요.
