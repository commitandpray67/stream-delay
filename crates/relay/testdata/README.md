# Test certificates

For the RTMPS tests in `src/egress.rs` only: a test CA (`test-ca.der`) and a
certificate for `localhost` and `127.0.0.1` it signed (`localhost.der`, with its
private key `localhost.key.der`, PKCS#8). They are trusted by nothing but those
tests, and valid until 2126. To make them again:

```sh
openssl ecparam -name prime256v1 -genkey -noout -out ca.key
openssl req -x509 -new -key ca.key -sha256 -days 36500 -subj "/CN=stream-delay test CA" \
  -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign,cRLSign" -out ca.pem
openssl ecparam -name prime256v1 -genkey -noout -out leaf.key
openssl req -new -key leaf.key -subj "/CN=localhost" -out leaf.csr
printf "subjectAltName=DNS:localhost,IP:127.0.0.1\nbasicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth\n" > ext
openssl x509 -req -in leaf.csr -CA ca.pem -CAkey ca.key -CAcreateserial -days 36500 -sha256 -extfile ext -out leaf.pem
openssl x509 -in ca.pem -outform DER -out test-ca.der
openssl x509 -in leaf.pem -outform DER -out localhost.der
openssl pkcs8 -topk8 -nocrypt -in leaf.key -outform DER -out localhost.key.der
```
