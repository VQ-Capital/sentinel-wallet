# 🏦 sentinel-treasury-vca (Legacy: sentinel-wallet)

**Domain:** Virtual Currency Accounting (VCA) & Reconciliation
**Rol:** Sistemin Muhasebecisi (Merkezi Hazine)

Her saniye tüm işlemlerin K/Z (PnL), komisyon ve açık pozisyon durumlarını hesaplayıp kasanın (Equity) anlık durumunu (Snapshot) yayınlar. Risk motoru, yatıracağı parayı ve kaldıracı bu servisten gelen onaya göre belirler.

- **NATS Çıktısı:** `wallet.equity.snapshot`