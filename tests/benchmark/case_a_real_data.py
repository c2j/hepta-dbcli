#!/usr/bin/env python3
"""
生成 Case A 真实数据（2000×4，qty–amount ρ≈0.79）。
输出：real.csv
"""
import numpy as np
import pandas as pd
from pathlib import Path

ROOT = Path(__file__).parent
OUT_CSV = ROOT / "real.csv"

def main():
    rng = np.random.default_rng(42)
    n = 2000

    # qty–amount: 双变量正态，rho=0.80，再做边际变换
    mean = [0.0, 0.0]
    cov = [[1.0, 0.80], [0.80, 1.0]]
    z = rng.multivariate_normal(mean, cov, size=n)

    # qty: 正态 → 截断整数 [1, 30]
    qty = np.clip(np.round(12 + 4 * z[:, 0]), 1, 30).astype(int)

    # amount: 正态 → 保留两位小数，均值 40, std 18
    amount = np.round(40 + 18 * z[:, 1], 2)

    # 独立类别
    category = rng.choice(["A", "B", "C", "D"], size=n, p=[0.4, 0.3, 0.2, 0.1])
    region = rng.choice(["east", "west", "south"], size=n, p=[0.5, 0.3, 0.2])

    df = pd.DataFrame({
        "qty": qty,
        "amount": amount,
        "category": category,
        "region": region
    })

    print(f"Generated {len(df)} rows")
    print(f"qty: {qty.min()}~{qty.max()}, mean={qty.mean():.2f}, std={qty.std():.2f}")
    print(f"amount: {amount.min():.2f}~{amount.max():.2f}, mean={amount.mean():.2f}, std={amount.std():.2f}")
    print(f"corr(qty,amount)={df['qty'].corr(df['amount']):.4f}")

    df.to_csv(OUT_CSV, index=False)
    print(f"Saved to {OUT_CSV}")

if __name__ == "__main__":
    main()