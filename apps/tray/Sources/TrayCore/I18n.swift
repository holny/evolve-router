import Foundation

public let LANGS = ["zh", "en", "ko", "ja", "es", "fr"]
public let LOCALE = ["zh": "zh-CN", "en": "en-US", "ko": "ko-KR", "ja": "ja-JP", "es": "es-ES", "fr": "fr-FR"]
public var lang: String = {
    let l = UserDefaults.standard.string(forKey: "ev-lang") ?? ""
    return LANGS.contains(l) ? l : "zh"
}()

let TOLD: [String: [String: String]] = [
    "title": ["zh": "EvolveRouter", "en": "EvolveRouter", "ko": "EvolveRouter", "ja": "EvolveRouter", "es": "EvolveRouter", "fr": "EvolveRouter"],
    "healthy": ["zh": "网关运行中 · 8787", "en": "Gateway running · 8787", "ko": "게이트웨이 실행 중 · 8787", "ja": "ゲートウェイ実行中 · 8787", "es": "Gateway en ejecución · 8787", "fr": "Gateway en cours · 8787"],
    "stopped": ["zh": "网关已停止", "en": "Gateway stopped", "ko": "게이트웨이 중지됨", "ja": "ゲートウェイ停止", "es": "Gateway detenido", "fr": "Gateway arrêté"],
    "starting": ["zh": "网关启动中…", "en": "Starting gateway…", "ko": "게이트웨이 시작 중…", "ja": "起動中…", "es": "Iniciando gateway…", "fr": "Démarrage du gateway…"],
    "external": ["zh": "外部实例运行中（为此面板只读）", "en": "External instance running (read-only here)", "ko": "외부 인스턴스 실행 중(읽기 전용)", "ja": "外部インスタンス実行中（読み取り専用）", "es": "Instancia externa en ejecución (solo lectura)", "fr": "Instance externe en cours (lecture seule)"],
    "inflight": ["zh": "进行中", "en": "in flight", "ko": "진행 중", "ja": "進行中", "es": "en vuelo", "fr": "en cours"],
    "copy_cfg": ["zh": "复制接入配置（baseURL / model / provider）", "en": "Copy wiring config (baseURL / model / provider)", "ko": "접속 설정 복사(baseURL/model/provider)", "ja": "接続設定をコピー（baseURL/model/provider）", "es": "Copiar configuración (baseURL/modelo/proveedor)", "fr": "Copier la config (baseURL/modèle/provider)"],
    "reqs": ["zh": "请求", "en": "reqs", "ko": "요청", "ja": "リクエスト", "es": "solicitudes", "fr": "requêtes"],
    "success_rate": ["zh": "成功率", "en": "success", "ko": "성공률", "ja": "成功率", "es": "éxito", "fr": "succès"],
    "tokens": ["zh": "Tokens 用量", "en": "tokens used", "ko": "토큰 사용량", "ja": "トークン使用量", "es": "tokens usados", "fr": "tokens utilisés"],
    "last_24h": ["zh": "最近 24h", "en": "last 24h", "ko": "최근 24h", "ja": "直近24h", "es": "últimas 24h", "fr": "dernières 24h"],
    "last_sync": ["zh": "最后同步", "en": "synced", "ko": "동기화", "ja": "同期", "es": "sincronizado", "fr": "synchronisé"],
    "plan_5h": ["zh": "方案 5h 用量", "en": "plan 5h usage", "ko": "플랜 5h 사용량", "ja": "プラン 5h 使用量", "es": "uso 5h del plan", "fr": "utilisation 5h du plan"],
    "recent": ["zh": "最近决策", "en": "recent decisions", "ko": "최근 결정", "ja": "最近の決定", "es": "decisiones recientes", "fr": "décisions récentes"],
    "fail_short": ["zh": "失败", "en": "fail", "ko": "실패", "ja": "失敗", "es": "fallo", "fr": "échec"],
    "providers": ["zh": "Provider 管理", "en": "Providers", "ko": "Provider 관리", "ja": "Provider 管理", "es": "Proveedores", "fr": "Providers"],
    "cooldown": ["zh": "冷却", "en": "cooldown", "ko": "쿨다운", "ja": "クールダウン", "es": "enfriamiento", "fr": "refroidissement"],
    "disabled": ["zh": "已禁用", "en": "disabled", "ko": "비활성", "ja": "無効", "es": "deshabilitado", "fr": "désactivé"],
    "payg": ["zh": "按量", "en": "payg", "ko": "종량", "ja": "従量", "es": "por uso", "fr": "à l'usage"],
    "autostart": ["zh": "开机自动启动", "en": "Launch at login", "ko": "로그인 시 자동 시작", "ja": "ログイン時に起動", "es": "Abrir al iniciar sesión", "fr": "Ouvrir à la connexion"],
    "stop_gw": ["zh": "停止网关", "en": "Stop gateway", "ko": "게이트웨이 중지", "ja": "ゲートウェイ停止", "es": "Detener gateway", "fr": "Arrêter le gateway"],
    "start_gw": ["zh": "启动网关", "en": "Start gateway", "ko": "게이트웨이 시작", "ja": "ゲートウェイ起動", "es": "Iniciar gateway", "fr": "Démarrer le gateway"],
    "dashboard": ["zh": "完整面板", "en": "Full dashboard", "ko": "전체 대시보드", "ja": "ダッシュボード", "es": "Panel completo", "fr": "Tableau complet"],
    "github": ["zh": "开源地址", "en": "GitHub", "ko": "GitHub", "ja": "GitHub", "es": "GitHub", "fr": "GitHub"],
    "quit": ["zh": "退出", "en": "Quit", "ko": "종료", "ja": "終了", "es": "Salir", "fr": "Quitter"],
    "quit_confirm": ["zh": "确定彻底退出 EvolveRouter 吗？", "en": "Quit EvolveRouter completely?", "ko": "EvolveRouter를 완전히 종료할까요?", "ja": "EvolveRouter を完全終了しますか？", "es": "¿Salir de EvolveRouter por completo?", "fr": "Quitter complètement EvolveRouter ?"],
    "quit_action": ["zh": "退出（网关与菜单栏将全部关闭）", "en": "Quit (gateway and menu bar app will both close)", "ko": "종료(게이트웨어와 메뉴바 모두 닫힘)", "ja": "終了（ゲートウェイとメニューバーも閉じる）", "es": "Salir (cierra gateway y barra de menús)", "fr": "Quitter (gateway et menu-bar fermés)"],
    "cancel": ["zh": "取消", "en": "Cancel", "ko": "취소", "ja": "キャンセル", "es": "Cancelar", "fr": "Annuler"],
    "quit_hint": ["zh": "重新打开方式：双击 应用程序 → EvolveRouter，或在终端执行 open -a EvolveRouter", "en": "Reopen: launchpad/Applications → EvolveRouter, or run open -a EvolveRouter", "ko": "다시 열기: 응용 프로그램 → EvolveRouter 실행", "ja": "再起動：アプリケーション → EvolveRouter", "es": "Reabrir: Aplicaciones → EvolveRouter o open -a EvolveRouter", "fr": "Réouverture : Applications → EvolveRouter ou open -a EvolveRouter"],
    "gateway_err": ["zh": "网关错误", "en": "gateway error", "ko": "게이트웨이 오류", "ja": "ゲートウェイエラー", "es": "error del gateway", "fr": "erreur gateway"],
    "plan_names": ["zh": "CP", "en": "CP", "ko": "CP", "ja": "CP", "es": "CP", "fr": "CP"],
]

public func t(_ key: String) -> String {
    TOLD[key]?[lang] ?? TOLD[key]?["zh"] ?? key
}
