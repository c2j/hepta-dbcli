#!/usr/bin/env python3

import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import yaml

import p2_patch_rules
import p2_make_staging
import p2_report


class PatchRulesTests(unittest.TestCase):
    def test_adds_required_rows_and_can_enable_child_zipf(self):
        source = """version: '1'
tables:
  - name: customer
    relationships: []
  - name: payment
    relationships: []
  - name: rental
    relationships: []
"""
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "rules.yaml"
            path.write_text(source, encoding="utf-8")
            p2_patch_rules.patch_rules(path, zipf=True)
            patched = yaml.load(path.read_text(encoding="utf-8"), Loader=yaml.BaseLoader)

        by_name = {table["name"]: table for table in patched["tables"]}
        self.assertEqual(by_name["customer"]["rows"], "599")
        self.assertEqual(by_name["rental"]["rows"], "16044")
        self.assertEqual(by_name["payment"]["rows"], "16049")
        self.assertEqual(by_name["customer"]["strategy"], "uniform")
        self.assertEqual(by_name["rental"]["strategy"], "zipf")
        self.assertEqual(by_name["payment"]["strategy"], "zipf")

    def test_drops_relationships_referencing_out_of_scope_tables(self):
        source = """version: '1'
tables:
  - name: customer
    relationships:
      - pk: address_id
        references: [address.address_id]
        pool_strategy: !projection
          unique: false
  - name: rental
    relationships:
      - pk: customer_id
        references: [customer.customer_id]
        pool_strategy: !projection
          unique: false
  - name: payment
    relationships: []
"""
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "rules.yaml"
            path.write_text(source, encoding="utf-8")
            p2_patch_rules.patch_rules(path, zipf=False)
            patched = yaml.load(path.read_text(encoding="utf-8"), Loader=p2_patch_rules.RulesLoader)

        by_name = {table["name"]: table for table in patched["tables"]}
        self.assertEqual(by_name["customer"]["relationships"], [])
        self.assertEqual(len(by_name["rental"]["relationships"]), 1)


class ReportGateTests(unittest.TestCase):
    def test_injected_orphan_and_bad_ks_fail_gates(self):
        inputs = p2_report.self_test_inputs()
        report = p2_report.evaluate_gates(inputs)
        self.assertFalse(report["passed"])
        self.assertFalse(report["P2-1"]["passed"])
        self.assertFalse(report["P2-2"]["passed"])
        self.assertGreaterEqual(report["P2-2"]["ks_statistic"], 0.15)


class StagingDdlTests(unittest.TestCase):
    def test_builds_tables_before_pk_and_fk_constraints(self):
        columns = [
            ["customer", "customer_id", "integer", "NO", ""],
            ["rental", "rental_id", "integer", "NO", ""],
            ["rental", "customer_id", "integer", "NO", ""],
            ["payment", "payment_id", "integer", "NO", ""],
            ["payment", "customer_id", "integer", "NO", ""],
            ["payment", "rental_id", "integer", "YES", ""],
        ]
        constraints = [
            ["customer", "customer_pkey", "p", "PRIMARY KEY (customer_id)"],
            ["rental", "rental_pkey", "p", "PRIMARY KEY (rental_id)"],
            ["payment", "payment_pkey", "p", "PRIMARY KEY (payment_id)"],
            ["rental", "rental_customer_fkey", "f", "FOREIGN KEY (customer_id) REFERENCES customer(customer_id)"],
            ["payment", "payment_rental_fkey", "f", "FOREIGN KEY (rental_id) REFERENCES rental(rental_id)"],
        ]
        with patch.object(p2_make_staging, "query_rows", side_effect=[columns, constraints]):
            ddl = p2_make_staging.build_ddl("public")

        self.assertIn('CREATE TABLE "staging"."customer"', ddl)
        self.assertIn('"customer_id" integer NOT NULL', ddl)
        self.assertIn('REFERENCES "staging".customer(customer_id)', ddl)
        self.assertLess(ddl.index('CREATE TABLE "staging"."payment"'), ddl.index("ADD CONSTRAINT"))

    def test_relaxes_not_null_on_defaultless_temporal_columns(self):
        columns = [
            ["customer", "create_date", "timestamp without time zone", "NO", "now()"],
            ["rental", "rental_id", "integer", "NO", ""],
            ["rental", "rental_date", "timestamp without time zone", "NO", ""],
            ["payment", "payment_id", "integer", "NO", ""],
        ]
        constraints = [["rental", "rental_pkey", "p", "PRIMARY KEY (rental_id)"]]
        with patch.object(p2_make_staging, "query_rows", side_effect=[columns, constraints]):
            ddl = p2_make_staging.build_ddl("public")

        self.assertIn('"rental_date" timestamp without time zone\n', ddl)
        self.assertIn('"create_date" timestamp without time zone DEFAULT now() NOT NULL', ddl)
        self.assertNotIn('"rental_date" timestamp without time zone NOT NULL', ddl)

    def test_skips_primary_keys_that_cover_temporal_columns(self):
        columns = [
            ["customer", "customer_id", "integer", "NO", ""],
            ["rental", "rental_id", "integer", "NO", ""],
            ["payment", "payment_id", "integer", "NO", ""],
            ["payment", "payment_date", "timestamp with time zone", "NO", ""],
        ]
        constraints = [
            ["customer", "customer_pkey", "p", "PRIMARY KEY (customer_id)"],
            ["payment", "payment_pkey", "p", "PRIMARY KEY (payment_date, payment_id)"],
        ]
        with patch.object(p2_make_staging, "query_rows", side_effect=[columns, constraints]):
            ddl = p2_make_staging.build_ddl("public")

        self.assertIn("customer_pkey", ddl)
        self.assertNotIn("payment_pkey", ddl)
        payment_table = ddl.split('"staging"."payment"')[1].split(");")[0]
        self.assertIn('"payment_id" integer NOT NULL', payment_table)
        self.assertIn('"payment_date" timestamp with time zone\n', payment_table)


if __name__ == "__main__":
    unittest.main()
