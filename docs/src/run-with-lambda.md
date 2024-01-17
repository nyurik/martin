## Using with AWS Lambda

Martin can be run in AWS Lambda. This is useful if you want to serve tiles from a serverless environment, while accessing "nearby" data from a PostgreSQL database or PMTiles file in S3, without exposing the raw file to the world to prevent download abuse and improve performance.

To run in a Lambda, you need to ...
