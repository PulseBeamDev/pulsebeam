
proto-ts:
	protoc --plugin=protoc-gen-ts_proto=./node_modules/.bin/protoc-gen-ts_proto \
				--ts_proto_out=. \
				proto/sfu.proto
