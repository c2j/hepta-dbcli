# P2 Multi-table FK Benchmark

Overall: **PASS**

## P2-0: PASS

```json
{
  "load_succeeded": true,
  "passed": true,
  "tables": {
    "customer": {
      "expected": 602,
      "generated_file": 602,
      "passed": true,
      "staging": 602
    },
    "payment": {
      "expected": 17673,
      "generated_file": 17673,
      "passed": true,
      "staging": 17673
    },
    "rental": {
      "expected": 17679,
      "generated_file": 17679,
      "passed": true,
      "staging": 17679
    }
  }
}
```

## P2-1: PASS

```json
{
  "orphan_counts": {
    "payment.customer_id->customer.customer_id": 0,
    "payment.rental_id->rental.rental_id": 0,
    "rental.customer_id->customer.customer_id": 0
  },
  "passed": true
}
```

## on_grid: PASS

```json
{
  "passed": true,
  "ratio": 1.0,
  "threshold": 0.95
}
```

## P2-2 fan-out KS (record only, not gated)

```json
{
  "gate": false,
  "ks_statistic": 0.7237936772046589,
  "threshold": 0.15,
  "within_threshold": false
}
```

## P2-3 (record only)

```json
{
  "gate": false,
  "marginals": {
    "customer.active": {
      "kind": "ks",
      "value": 0.0049833887043189366
    },
    "customer.activebool": {
      "kind": "tv",
      "value": 0.00166112956810629
    },
    "customer.address_id": {
      "kind": "ks",
      "value": 0.5132890365448505
    },
    "customer.create_date": {
      "kind": "tv",
      "value": 0.06478405315614619
    },
    "customer.customer_id": {
      "kind": "ks",
      "value": 0.9933554817275747
    },
    "customer.email": {
      "kind": "tv",
      "value": 0.9169435215946907
    },
    "customer.first_name": {
      "kind": "tv",
      "value": 0.9003322259136284
    },
    "customer.last_name": {
      "kind": "tv",
      "value": 0.9169435215946907
    },
    "customer.last_update": {
      "kind": "tv",
      "value": 1.0000000000000047
    },
    "customer.store_id": {
      "kind": "ks",
      "value": 0.011627906976744186
    },
    "payment.amount": {
      "kind": "ks",
      "value": 0.10032252588694623
    },
    "payment.customer_id": {
      "kind": "ks",
      "value": 0.878288915294517
    },
    "payment.payment_date": {
      "kind": "tv",
      "value": null
    },
    "payment.payment_id": {
      "kind": "ks",
      "value": 0.4768290612799185
    },
    "payment.rental_id": {
      "kind": "ks",
      "value": 0.8756860748033723
    },
    "payment.staff_id": {
      "kind": "ks",
      "value": 0.10428337011260114
    },
    "rental.customer_id": {
      "kind": "ks",
      "value": 0.8777080151592285
    },
    "rental.inventory_id": {
      "kind": "ks",
      "value": 0.48764070365970924
    },
    "rental.last_update": {
      "kind": "tv",
      "value": 1.0000000000001827
    },
    "rental.rental_date": {
      "kind": "tv",
      "value": null
    },
    "rental.rental_id": {
      "kind": "ks",
      "value": 0.9074042649471123
    },
    "rental.return_date": {
      "kind": "tv",
      "value": null
    },
    "rental.staff_id": {
      "kind": "ks",
      "value": 0.08111318513490573
    }
  },
  "sdv_gc_reference": {
    "enabled": false
  }
}
```

## P2-4 (record only)

```json
{
  "gate": false,
  "store_id_vs_per_customer_avg_amount_pearson": {
    "absolute_error": 0.5581464449470551,
    "real": 0.5512965813908443,
    "synthetic": -0.006849863556210893
  }
}
```
