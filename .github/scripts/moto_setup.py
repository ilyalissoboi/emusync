"""Prepare a moto S3 server for the gated S3 tests.

moto must run with INITIAL_NO_AUTH_ACTION_COUNT=3: the three IAM calls below go through
unauthenticated, and everything after them (including the tests) is signature-checked
against the access key created here. Any other HTTP request to the server before this script
(a curl readiness probe, say) uses up one of those three, so this script waits for the port
with a bare TCP connect instead.

Usage: python moto_setup.py [endpoint] [bucket]
Prints AWS_ACCESS_KEY_ID=... and AWS_SECRET_ACCESS_KEY=... lines (suitable for $GITHUB_ENV).
"""

import json
import socket
import sys
import time
from urllib.parse import urlparse

import boto3

endpoint = sys.argv[1] if len(sys.argv) > 1 else "http://127.0.0.1:5056"
bucket = sys.argv[2] if len(sys.argv) > 2 else "emusync-test"
region = "us-east-1"

url = urlparse(endpoint)
for _ in range(60):
    try:
        socket.create_connection((url.hostname, url.port), timeout=1).close()
        break
    except OSError:
        time.sleep(0.5)
else:
    sys.exit(f"moto server at {endpoint} did not come up")
bootstrap = dict(endpoint_url=endpoint, region_name=region, aws_access_key_id="bootstrap",
                 aws_secret_access_key="bootstrap")

iam = boto3.client("iam", **bootstrap)
iam.create_user(UserName="emusync-ci")
key = iam.create_access_key(UserName="emusync-ci")["AccessKey"]
iam.put_user_policy(
    UserName="emusync-ci",
    PolicyName="s3-all",
    PolicyDocument=json.dumps({
        "Version": "2012-10-17",
        "Statement": [{"Effect": "Allow", "Action": "s3:*", "Resource": "*"}],
    }),
)

s3 = boto3.client("s3", endpoint_url=endpoint, region_name=region, aws_access_key_id=key["AccessKeyId"],
                  aws_secret_access_key=key["SecretAccessKey"])
s3.create_bucket(Bucket=bucket)

print(f"AWS_ACCESS_KEY_ID={key['AccessKeyId']}")
print(f"AWS_SECRET_ACCESS_KEY={key['SecretAccessKey']}")
